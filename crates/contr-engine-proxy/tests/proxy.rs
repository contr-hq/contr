#![allow(clippy::expect_used, reason = "test helpers")]

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use contr_engine_proxy::Rewrites;
use http::{HeaderValue, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(10);

/// Fake engine: answers with the request body, and turns `/attach` into an upgraded stream that echoes everything
/// the client sends once the client half-closes.
async fn engine(stream: DuplexStream) {
    let service = service_fn(|mut request: Request<Incoming>| async move {
        if request.uri().path().ends_with("/attach") {
            let upgrade = hyper::upgrade::on(&mut request);
            tokio::spawn(async move {
                let mut io = TokioIo::new(upgrade.await.expect("engine side upgrades"));
                let mut received = Vec::new();
                io.read_to_end(&mut received).await.expect("client half-closes");
                io.write_all(&received).await.expect("echo is written");
                io.shutdown().await.expect("engine half-closes");
            });
            let mut response = Response::new(Full::new(Bytes::new()));
            *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
            response
                .headers_mut()
                .insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
            response
                .headers_mut()
                .insert(header::UPGRADE, HeaderValue::from_static("tcp"));
            return Ok::<_, Infallible>(response);
        }
        let body = request.into_body().collect().await.expect("request body").to_bytes();
        Ok(Response::new(Full::new(body)))
    });

    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await;
}

async fn connected_client() -> hyper::client::conn::http1::SendRequest<Full<Bytes>> {
    let (client, proxy) = duplex(64 * 1024);
    let connect = || async {
        let (proxy_end, engine_end) = duplex(64 * 1024);
        tokio::spawn(engine(engine_end));
        Ok::<_, std::io::Error>(proxy_end)
    };
    let rewrites = Arc::new(Rewrites {
        drives_root: "/mnt".to_owned(),
        distro_root: None,
        host_alias: true,
    });
    tokio::spawn(contr_engine_proxy::serve(proxy, connect, rewrites));

    let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(client))
        .await
        .expect("client handshake");
    tokio::spawn(connection.with_upgrades());
    sender
}

fn post(path: &str, body: &str) -> Request<Full<Bytes>> {
    Request::post(path)
        .header(header::HOST, "docker")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body.to_owned())))
        .expect("request builds")
}

#[tokio::test]
async fn container_definitions_reach_the_engine_rewritten() {
    let mut client = connected_client().await;
    let definition = json!({"Image": "alpine", "HostConfig": {"Binds": [r"C:\src:/w:ro"]}}).to_string();

    let response = timeout(
        LIMIT,
        client.send_request(post("/v1.52/containers/create", &definition)),
    )
    .await
    .expect("answer in time")
    .expect("request succeeds");
    let received: Value = serde_json::from_slice(&response.into_body().collect().await.expect("body").to_bytes())
        .expect("engine received JSON");

    assert_eq!(received["HostConfig"]["Binds"], json!(["/mnt/c/src:/w:ro"]));
    assert_eq!(
        received["HostConfig"]["ExtraHosts"],
        json!(["host.docker.internal:host-gateway"])
    );
}

#[tokio::test]
async fn other_requests_pass_through_on_one_client_connection() {
    let mut client = connected_client().await;

    for body in ["first", "second"] {
        let response = timeout(LIMIT, client.send_request(post("/v1.52/containers/abc/start", body)))
            .await
            .expect("answer in time")
            .expect("request succeeds");
        let echoed = response.into_body().collect().await.expect("body").to_bytes();
        assert_eq!(echoed, body.as_bytes());
    }
}

#[tokio::test]
async fn upgraded_streams_carry_half_close_both_ways() {
    let mut client = connected_client().await;
    let request = Request::post("/v1.52/containers/abc/attach?stream=1&stdin=1")
        .header(header::HOST, "docker")
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "tcp")
        .body(Full::new(Bytes::new()))
        .expect("request builds");

    let response = timeout(LIMIT, client.send_request(request))
        .await
        .expect("answer in time")
        .expect("request succeeds");
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    let mut stream = TokioIo::new(hyper::upgrade::on(response).await.expect("client side upgrades"));

    stream.write_all(b"stdin bytes").await.expect("stdin is written");
    stream.shutdown().await.expect("client half-closes");
    let mut echoed = Vec::new();
    timeout(LIMIT, stream.read_to_end(&mut echoed))
        .await
        .expect("engine output ends in time")
        .expect("output is read");

    assert_eq!(echoed, b"stdin bytes");
}
