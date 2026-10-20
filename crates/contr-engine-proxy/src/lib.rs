//! Docker Engine API proxy between Windows clients and an engine in a Linux guest. Container definitions get guest
//! paths for Windows bind sources and a `host.docker.internal` alias; every other request, upgrades included, passes
//! through unchanged.

#![warn(missing_docs)]

mod rewrite;

use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderValue, Request, Response, StatusCode, header};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};

pub use rewrite::Rewrites;

/// Largest container definition the proxy reads to rewrite.
const MAX_DEFINITION: usize = 16 * 1024 * 1024;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Body = BoxBody<Bytes, BoxError>;

/// Failure to forward one request; the client receives `502 Bad Gateway`.
#[derive(Debug, thiserror::Error)]
enum ProxyError {
    #[error("cannot reach the engine: {0}")]
    Connect(#[from] io::Error),
    #[error(transparent)]
    Http(#[from] hyper::Error),
    #[error("cannot read the request body: {0}")]
    Body(BoxError),
}

/// Serves one client connection until it closes; each request travels on a fresh engine connection from
/// `connect`, which keeps upgraded streams on the connection that negotiated them.
pub async fn serve<C, F, Fut, E>(client: C, connect: F, rewrites: Arc<Rewrites>) -> Result<(), hyper::Error>
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = io::Result<E>> + Send,
    E: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |request| {
        let connect = connect.clone();
        let rewrites = Arc::clone(&rewrites);
        async move {
            Ok::<_, Infallible>(
                forward(request, connect, &rewrites)
                    .await
                    .unwrap_or_else(|error| bad_gateway(&error)),
            )
        }
    });

    hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(client), service)
        .with_upgrades()
        .await
}

async fn forward<F, Fut, E>(
    mut request: Request<Incoming>,
    connect: F,
    rewrites: &Rewrites,
) -> Result<Response<Body>, ProxyError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = io::Result<E>>,
    E: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let client_upgrade = request
        .headers()
        .contains_key(header::UPGRADE)
        .then(|| hyper::upgrade::on(&mut request));
    let request = if rewrite::targets(request.method(), request.uri().path()) {
        rewrite_definition(request, rewrites).await?
    } else {
        request.map(|body| body.map_err(BoxError::from).boxed())
    };

    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(connect().await?)).await?;
    tokio::spawn(async move {
        if let Err(error) = connection.with_upgrades().await {
            tracing::debug!(%error, "engine connection ended");
        }
    });
    let mut response = sender.send_request(request).await?;

    if let Some(client_upgrade) = client_upgrade.filter(|_| response.status() == StatusCode::SWITCHING_PROTOCOLS) {
        let engine_upgrade = hyper::upgrade::on(&mut response);
        tokio::spawn(async move {
            match tokio::try_join!(client_upgrade, engine_upgrade) {
                Ok((client, engine)) => {
                    let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut TokioIo::new(engine)).await;
                }
                Err(error) => tracing::debug!(%error, "cannot complete the upgrade"),
            }
        });
    }

    Ok(response.map(|body| body.map_err(BoxError::from).boxed()))
}

async fn rewrite_definition(request: Request<Incoming>, rewrites: &Rewrites) -> Result<Request<Body>, ProxyError> {
    let (mut parts, body) = request.into_parts();
    let original = Limited::new(body, MAX_DEFINITION)
        .collect()
        .await
        .map_err(ProxyError::Body)?
        .to_bytes();
    let body = rewrite::container_create(&original, rewrites).map_or(original, Bytes::from);

    parts.headers.remove(header::TRANSFER_ENCODING);
    parts
        .headers
        .insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    Ok(Request::from_parts(
        parts,
        Full::new(body).map_err(|never| match never {}).boxed(),
    ))
}

fn bad_gateway(error: &ProxyError) -> Response<Body> {
    tracing::warn!(%error, "cannot forward a Docker API request");
    let mut response = Response::new(
        Full::new(Bytes::from(
            serde_json::json!({ "message": error.to_string() }).to_string(),
        ))
        .map_err(|never| match never {})
        .boxed(),
    );
    *response.status_mut() = StatusCode::BAD_GATEWAY;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}
