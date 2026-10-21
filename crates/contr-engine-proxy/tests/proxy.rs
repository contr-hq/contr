#![allow(clippy::expect_used, clippy::unwrap_used, reason = "test helpers")]

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use contr_core::proxy::Proxy;
use contr_engine_proxy::Rewrites;
use http::{HeaderValue, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(10);
const PROXY_URL: &str = "http://198.19.255.254:3128";

/// Fake engine: answers `/build` with its query and other requests with their body. It turns `/attach` into an
/// upgraded stream that echoes everything the client sends once the client half-closes, and `/grpc` into HTTP/2
/// that answers every call with its request body.
async fn engine(stream: DuplexStream) {
    let service = service_fn(|mut request: Request<Incoming>| async move {
        if request.uri().path().ends_with("/grpc") {
            let upgrade = hyper::upgrade::on(&mut request);
            tokio::spawn(async move {
                let io = upgrade.await.expect("engine side upgrades");
                let echo = service_fn(|call: Request<Incoming>| async move {
                    let body = call.into_body().collect().await?.to_bytes();
                    Ok::<_, hyper::Error>(Response::new(Full::new(body)))
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(io, echo)
                    .await;
            });
            return Ok::<_, Infallible>(switching("h2c"));
        }
        if request.uri().path().ends_with("/build") {
            let query = request.uri().query().unwrap_or_default().to_owned();
            return Ok(Response::new(Full::new(Bytes::from(query))));
        }
        if request.uri().path().ends_with("/attach") {
            let upgrade = hyper::upgrade::on(&mut request);
            tokio::spawn(async move {
                let mut io = TokioIo::new(upgrade.await.expect("engine side upgrades"));
                let mut received = Vec::new();
                io.read_to_end(&mut received).await.expect("client half-closes");
                io.write_all(&received).await.expect("echo is written");
                io.shutdown().await.expect("engine half-closes");
            });
            return Ok(switching("tcp"));
        }
        let body = request.into_body().collect().await.expect("request body").to_bytes();
        Ok(Response::new(Full::new(body)))
    });

    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await;
}

fn switching(protocol: &'static str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    response
        .headers_mut()
        .insert(header::UPGRADE, HeaderValue::from_static(protocol));
    response
}

async fn connected_client() -> hyper::client::conn::http1::SendRequest<Full<Bytes>> {
    connected_client_with(None).await
}

async fn proxied_client() -> hyper::client::conn::http1::SendRequest<Full<Bytes>> {
    connected_client_with(Some(Proxy {
        http: Some(PROXY_URL.to_owned()),
        https: None,
        exclude: vec!["localhost".to_owned()],
    }))
    .await
}

async fn connected_client_with(proxy: Option<Proxy>) -> hyper::client::conn::http1::SendRequest<Full<Bytes>> {
    let (client, served) = duplex(64 * 1024);
    let connect = || async {
        let (proxy_end, engine_end) = duplex(64 * 1024);
        tokio::spawn(engine(engine_end));
        Ok::<_, std::io::Error>(proxy_end)
    };
    let rewrites = Arc::new(Rewrites {
        drives_root: "/mnt".to_owned(),
        distro_root: None,
        host_alias: true,
        proxy,
    });
    tokio::spawn(contr_engine_proxy::serve(served, connect, rewrites));

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

async fn send(
    client: &mut hyper::client::conn::http1::SendRequest<Full<Bytes>>,
    request: Request<Full<Bytes>>,
) -> Response<Incoming> {
    timeout(LIMIT, client.send_request(request))
        .await
        .expect("answer in time")
        .expect("request succeeds")
}

#[tokio::test]
async fn new_containers_receive_the_proxy() {
    let mut client = proxied_client().await;
    let definition = json!({"Image": "alpine", "Env": ["A=b"]}).to_string();

    let response = send(&mut client, post("/v1.52/containers/create", &definition)).await;
    let received: Value = serde_json::from_slice(&response.into_body().collect().await.expect("body").to_bytes())
        .expect("engine received JSON");

    assert_eq!(
        received["Env"],
        json!([
            "A=b",
            format!("HTTP_PROXY={PROXY_URL}"),
            format!("http_proxy={PROXY_URL}"),
            "NO_PROXY=localhost",
            "no_proxy=localhost",
        ])
    );
}

#[tokio::test]
async fn classic_builds_receive_the_proxy_as_build_arguments() {
    let mut client = proxied_client().await;

    let response = send(&mut client, post("/v1.52/build?t=app&buildargs=%7B%7D", "")).await;
    let query = response.into_body().collect().await.expect("body").to_bytes();

    let args = form_urlencoded::parse(&query)
        .find(|(name, _)| name == "buildargs")
        .map(|(_, args)| serde_json::from_str::<Value>(&args).expect("JSON build arguments"))
        .expect("build arguments reach the engine");
    assert_eq!(args["http_proxy"], PROXY_URL);
    assert!(query.starts_with(b"t=app&"));
}

#[tokio::test]
async fn buildkit_sessions_receive_the_proxy_as_build_arguments() {
    let mut client = proxied_client().await;
    let request = Request::post("/v1.52/grpc")
        .header(header::HOST, "docker")
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "h2c")
        .body(Full::new(Bytes::new()))
        .expect("request builds");

    let response = send(&mut client, request).await;
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    let upgraded = hyper::upgrade::on(response).await.expect("client side upgrades");
    let (mut calls, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), upgraded)
        .await
        .expect("HTTP/2 handshake");
    tokio::spawn(connection);

    // A gateway Solve request that names the Dockerfile frontend, with no declared length, as gRPC clients send it.
    let mut call = vec![0, 0, 0, 0, 15, 0x12, 13];
    call.extend_from_slice(b"dockerfile.v0");
    let body = futures_util::stream::iter([Ok::<_, Infallible>(Frame::data(Bytes::from(call.clone())))]);
    let solve = Request::post("http://localhost/moby.buildkit.v1.frontend.LLBBridge/Solve")
        .header(header::CONTENT_TYPE, "application/grpc")
        .body(StreamBody::new(body))
        .expect("call builds");
    let response = timeout(LIMIT, calls.send_request(solve))
        .await
        .expect("answer in time")
        .expect("call succeeds");
    let received = response.into_body().collect().await.expect("body").to_bytes();

    assert!(received.len() > call.len());
    assert!(received.starts_with(&[0]));
    let needle = format!("build-arg:http_proxy\x12\x1a{PROXY_URL}");
    assert!(received.windows(needle.len()).any(|window| window == needle.as_bytes()));
}
