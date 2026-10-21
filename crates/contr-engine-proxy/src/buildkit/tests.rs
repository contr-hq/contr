use std::time::Duration;

use bytes::Bytes;
use contr_core::proxy::Proxy;
use http::{HeaderMap, HeaderValue, Request, Response};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

use super::frame::PRIORITY;
use super::*;

const GATEWAY_SOLVE_PATH: &str = "/moby.buildkit.v1.frontend.LLBBridge/Solve";
const PROXY_URL: &str = "http://198.19.255.254:3128";

fn variables() -> ProxyVariables {
    ProxyVariables::of(&Proxy {
        http: Some(PROXY_URL.to_owned()),
        https: None,
        exclude: Vec::new(),
    })
    .unwrap()
}

fn put_bytes(out: &mut Vec<u8>, number: u8, value: &[u8]) {
    out.push((number << 3) | 2);
    let mut length = value.len();
    while length >= 0x80 {
        out.push(u8::try_from(length & 0x7f).unwrap() | 0x80);
        length >>= 7;
    }
    out.push(u8::try_from(length).unwrap());
    out.extend_from_slice(value);
}

/// A framed gateway Solve call naming the Dockerfile frontend, with a definition of `definition` bytes.
fn solve_call(definition: usize) -> Vec<u8> {
    let mut message = Vec::new();
    put_bytes(&mut message, 1, &vec![7; definition]);
    put_bytes(&mut message, 2, b"dockerfile.v0");
    let mut call = vec![0];
    call.extend_from_slice(&u32::try_from(message.len()).unwrap().to_be_bytes());
    call.extend_from_slice(&message);
    call
}

fn passes_the_proxy(body: &[u8]) -> bool {
    let needle = format!(
        "build-arg:http_proxy\x12{}{PROXY_URL}",
        char::from(u8::try_from(PROXY_URL.len()).unwrap())
    );
    body.windows(needle.len()).any(|window| window == needle.as_bytes())
}

/// A HEADERS frame whose block names `path` as a literal the decoder does not index.
fn request_headers(stream: u32, path: &str, flags: u8) -> Frame {
    let mut block = vec![0x83, 0x04, u8::try_from(path.len()).unwrap()];
    block.extend_from_slice(path.as_bytes());
    headers(stream, block, flags)
}

fn headers(stream: u32, block: Vec<u8>, flags: u8) -> Frame {
    Frame {
        kind: HEADERS,
        flags: flags | END_HEADERS,
        stream,
        payload: block,
    }
}

fn data(stream: u32, flags: u8, payload: &[u8]) -> Frame {
    Frame {
        kind: DATA,
        flags,
        stream,
        payload: payload.to_vec(),
    }
}

fn open_flow() -> Flow {
    let mut flow = Flow::new();
    flow.connection = 1 << 20;
    flow.initial_stream = 1 << 20;
    flow
}

#[test]
fn holds_solve_calls_until_they_end() {
    let variables = variables();
    let mut calls = Calls::new(&variables);
    let mut flow = open_flow();
    let call = solve_call(10);

    assert_eq!(
        calls.pass(request_headers(1, GATEWAY_SOLVE_PATH, 0), &mut flow).len(),
        1
    );
    assert!(calls.pass(data(1, 0, &call[..4]), &mut flow).is_empty());
    let sent = calls.pass(data(1, END_STREAM, &call[4..]), &mut flow);

    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].flags, END_STREAM);
    assert!(passes_the_proxy(&sent[0].payload));
    assert!(flow.owed > 0);
    assert_eq!(
        flow.connection,
        (1 << 20) - i64::try_from(sent[0].payload.len()).unwrap()
    );
}

#[test]
fn passes_other_calls_at_once() {
    let variables = variables();
    let mut calls = Calls::new(&variables);
    let mut flow = open_flow();

    calls.pass(request_headers(1, "/moby.buildkit.v1.Control/Status", 0), &mut flow);
    let sent = calls.pass(data(1, END_STREAM, &solve_call(10)), &mut flow);

    assert_eq!(sent, [data(1, END_STREAM, &solve_call(10))]);
    assert_eq!(flow.owed, 0);
}

#[test]
fn releases_calls_it_cannot_rewrite_unchanged() {
    let variables = variables();
    let call = solve_call(10);

    let mut calls = Calls::new(&variables);
    let mut flow = open_flow();
    calls.pass(request_headers(1, GATEWAY_SOLVE_PATH, 0), &mut flow);
    calls.pass(data(1, 0, &call), &mut flow);
    let reset = Frame {
        kind: RST_STREAM,
        flags: 0,
        stream: 1,
        payload: vec![0, 0, 0, 8],
    };
    let sent = calls.pass(reset.clone(), &mut flow);
    assert_eq!(sent, [data(1, 0, &call), reset]);

    calls.pass(request_headers(3, GATEWAY_SOLVE_PATH, 0), &mut flow);
    let padded = data(3, END_STREAM | PADDED, &[0]);
    assert_eq!(calls.pass(padded.clone(), &mut flow), [padded]);

    flow.connection = 10;
    calls.pass(request_headers(5, GATEWAY_SOLVE_PATH, 0), &mut flow);
    assert_eq!(
        calls.pass(data(5, END_STREAM, &call), &mut flow),
        [data(5, END_STREAM, &call)]
    );
    assert_eq!(flow.owed, 0);
}

#[test]
fn passes_calls_that_declare_their_length_at_once() {
    let variables = variables();
    let mut calls = Calls::new(&variables);
    let mut flow = open_flow();
    let mut block = request_headers(1, GATEWAY_SOLVE_PATH, 0).payload;
    // content-length (static entry 28): 64, not indexed.
    block.extend_from_slice(&[0x0f, 0x0d, 0x02, b'6', b'4']);

    calls.pass(headers(1, block, 0), &mut flow);
    let call = data(1, END_STREAM, &solve_call(10));

    assert_eq!(calls.pass(call.clone(), &mut flow), [call]);
}

#[test]
fn stops_holding_calls_after_an_unreadable_header_block() {
    let variables = variables();
    let mut calls = Calls::new(&variables);
    let mut flow = open_flow();
    let garbage = Frame {
        kind: HEADERS,
        flags: END_HEADERS | PRIORITY,
        stream: 1,
        payload: vec![0, 0, 0, 0, 0, 0xbe],
    };

    assert_eq!(calls.pass(garbage.clone(), &mut flow), [garbage]);
    calls.pass(request_headers(3, GATEWAY_SOLVE_PATH, 0), &mut flow);
    let call = data(3, END_STREAM, &solve_call(10));
    assert_eq!(calls.pass(call.clone(), &mut flow), [call]);
}

#[test]
fn withholds_what_calls_grew_by_from_window_updates() {
    let mut flow = open_flow();
    flow.owed = 100;
    let mut update = |stream: u32, increment: u32| {
        let mut frame = Frame {
            kind: WINDOW_UPDATE,
            flags: 0,
            stream,
            payload: increment.to_be_bytes().to_vec(),
        };
        flow.received(&mut frame)
            .then(|| u32::from_be_bytes(frame.payload.try_into().unwrap()))
    };

    assert_eq!(update(0, 60), None);
    assert_eq!(update(0, 60), Some(20));
    assert_eq!(update(0, 60), Some(60));
    assert_eq!(update(7, 5), Some(5));
    assert_eq!(flow.connection, (1 << 20) + 180);
}

#[test]
fn settings_change_the_windows_of_held_streams() {
    let mut flow = Flow::new();
    flow.streams.insert(1, DEFAULT_WINDOW - 10);
    let mut settings = Frame {
        kind: SETTINGS,
        flags: 0,
        stream: 0,
        payload: vec![0, 4, 0, 1, 0, 0],
    };

    assert!(flow.received(&mut settings));
    assert_eq!(flow.initial_stream, 65_536);
    assert_eq!(flow.streams[&1], 65_536 - 10);
}

async fn echo(request: Request<Incoming>) -> Result<Response<BoxBody>, hyper::Error> {
    let body = request.into_body().collect().await?.to_bytes();
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", HeaderValue::from_static("0"));
    let frames = futures_util::stream::iter([
        Ok::<_, hyper::Error>(hyper::body::Frame::data(body)),
        Ok(hyper::body::Frame::trailers(trailers)),
    ]);

    Ok(Response::new(BodyExt::boxed(StreamBody::new(frames))))
}

type BoxBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;

/// Sends `body` to `path` through the relay, without a declared length as gRPC clients do, and returns the body
/// the engine received and the response's trailers.
async fn call(
    sender: &mut hyper::client::conn::http2::SendRequest<BoxBody>,
    path: &str,
    body: Vec<u8>,
) -> (Bytes, HeaderMap) {
    let frames = futures_util::stream::iter([Ok::<_, hyper::Error>(hyper::body::Frame::data(Bytes::from(body)))]);
    let request = Request::post(format!("http://localhost{path}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(BodyExt::boxed(StreamBody::new(frames)))
        .unwrap();
    let collected = sender
        .send_request(request)
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap();
    let trailers = collected.trailers().cloned().unwrap_or_default();

    (collected.to_bytes(), trailers)
}

#[tokio::test]
async fn relays_an_http2_session_and_rewrites_its_solve_calls() {
    let (client, relay_client) = duplex(64 * 1024);
    let (relay_engine, engine) = duplex(64 * 1024);
    let relayed = tokio::spawn(relay(relay_client, relay_engine, variables()));
    tokio::spawn(
        hyper::server::conn::http2::Builder::new(TokioExecutor::new())
            .serve_connection(TokioIo::new(engine), service_fn(echo)),
    );
    let (mut sender, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(client))
        .await
        .unwrap();
    tokio::spawn(connection);

    // Each build argument is a map entry holding two length-delimited strings.
    let entry = 2 + (2 + "build-arg:HTTP_PROXY".len()) + (2 + PROXY_URL.len());
    let session = async {
        let (status, trailers) = call(&mut sender, "/moby.buildkit.v1.Control/Status", b"status".to_vec()).await;
        assert_eq!(&status[..], b"status");
        assert_eq!(trailers["grpc-status"], "0");

        // Repeated calls reach the engine's dynamic table, and their windows need refilling with the extra withheld.
        for definition in [10, 40_000, 10, 200_000, 10] {
            for _ in 0..12 {
                let (solve, trailers) = call(&mut sender, GATEWAY_SOLVE_PATH, solve_call(definition)).await;
                assert!(passes_the_proxy(&solve), "a {definition}-byte definition");
                assert_eq!(solve.len(), solve_call(definition).len() + 2 * entry);
                assert_eq!(trailers["grpc-status"], "0");
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(30), session)
        .await
        .expect("the session stalled");

    drop(sender);
    tokio::time::timeout(Duration::from_secs(5), relayed)
        .await
        .expect("the relay did not end")
        .unwrap()
        .ok();
}

#[tokio::test]
async fn copies_what_is_not_http2() {
    const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: engine\r\n\r\n";
    let (mut client, relay_client) = duplex(1024);
    let (relay_engine, mut engine) = duplex(1024);
    tokio::spawn(relay(relay_client, relay_engine, variables()));

    client.write_all(REQUEST).await.unwrap();
    let mut request = [0; REQUEST.len()];
    engine.read_exact(&mut request).await.unwrap();
    engine.write_all(b"ok").await.unwrap();
    let mut response = [0; 2];
    client.read_exact(&mut response).await.unwrap();

    assert_eq!(&request, REQUEST);
    assert_eq!(&response, b"ok");
}
