#![allow(clippy::expect_used, reason = "test helpers")]

use std::time::Duration;

use contr_mux::{Channel, MuxStream, Role};
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(20);

async fn echo(_channel: Channel, stream: MuxStream) {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let _ = tokio::io::copy(&mut reader, &mut writer).await;
    let _ = writer.shutdown().await;
}

/// Answers a `HostTcp` stream with the port it names.
async fn report_port(channel: Channel, mut stream: MuxStream) {
    assert_eq!(channel, Channel::HostTcp);
    let port = contr_mux::read_port(&mut stream).await.expect("port arrives");
    stream.write_all(&port.to_be_bytes()).await.expect("port is echoed");
    stream.shutdown().await.expect("stream closes");
}

async fn ignore(_channel: Channel, _stream: MuxStream) {}

fn payload(seed: u8, len: usize) -> Vec<u8> {
    (0..=250u8)
        .cycle()
        .take(len)
        .map(|byte| byte.wrapping_add(seed))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_streams_echo_large_payloads_concurrently() {
    let (host, guest) = duplex(64 * 1024);
    let (_guest_opener, guest_driver) = contr_mux::session(guest, Role::Guest, echo);
    let guest_driver = tokio::spawn(guest_driver);
    let (opener, driver) = contr_mux::session(host, Role::Host, ignore);
    let driver = tokio::spawn(driver);

    let mut transfers = Vec::new();
    for seed in 0..8u8 {
        let opener = opener.clone();
        transfers.push(tokio::spawn(async move {
            let stream = opener.open(Channel::Engine).await.expect("stream opens");
            let (mut reader, mut writer) = tokio::io::split(stream);
            let sent = payload(seed, 2 * 1024 * 1024);
            let expected = sent.clone();
            let write = tokio::spawn(async move {
                writer.write_all(&sent).await.expect("payload is written");
                writer.shutdown().await.expect("stream half-closes");
            });
            let mut received = Vec::new();
            reader.read_to_end(&mut received).await.expect("echo is read");
            write.await.expect("writer finishes");
            assert_eq!(received, expected);
        }));
    }
    for transfer in transfers {
        timeout(LIMIT, transfer)
            .await
            .expect("transfer in time")
            .expect("transfer succeeds");
    }

    drop(opener);
    timeout(LIMIT, driver)
        .await
        .expect("host closes")
        .expect("host task")
        .expect("clean close");
    timeout(LIMIT, guest_driver)
        .await
        .expect("guest sees the close")
        .expect("guest task")
        .expect("clean end");
}

#[tokio::test]
async fn guest_opens_host_tcp_streams_with_their_port() {
    let (host, guest) = duplex(64 * 1024);
    let (host_opener, host_driver) = contr_mux::session(host, Role::Host, report_port);
    let host_driver = tokio::spawn(host_driver);
    let (guest_opener, guest_driver) = contr_mux::session(guest, Role::Guest, ignore);
    let guest_driver = tokio::spawn(guest_driver);

    let mut stream = guest_opener.open_host_tcp(8099).await.expect("stream opens");
    let mut port = [0; 2];
    timeout(LIMIT, stream.read_exact(&mut port))
        .await
        .expect("answer in time")
        .expect("port is read");
    assert_eq!(u16::from_be_bytes(port), 8099);

    drop(guest_opener);
    drop(host_opener);
    timeout(LIMIT, host_driver)
        .await
        .expect("host closes")
        .expect("host task")
        .expect("clean close");
    timeout(LIMIT, guest_driver)
        .await
        .expect("guest ends")
        .expect("guest task")
        .expect("clean end");
}

#[tokio::test]
async fn opening_after_the_peer_is_gone_fails() {
    let (host, guest) = duplex(1024);
    drop(guest);
    let (opener, driver) = contr_mux::session(host, Role::Host, ignore);
    let driver = tokio::spawn(driver);

    let result = timeout(LIMIT, opener.open(Channel::Engine))
        .await
        .expect("open resolves");
    assert!(result.is_err());
    let _ = timeout(LIMIT, driver).await.expect("driver ends");
}
