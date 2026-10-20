use std::time::Duration;

use contr_mux::{Channel, MuxStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(20);

async fn echo(channel: Channel, stream: MuxStream) {
    assert_eq!(channel, Channel::Engine);
    let (mut reader, mut writer) = tokio::io::split(stream);
    let _ = tokio::io::copy(&mut reader, &mut writer).await;
    let _ = writer.shutdown().await;
}

fn payload(seed: u8, len: usize) -> Vec<u8> {
    (0..=250u8)
        .cycle()
        .take(len)
        .map(|byte| byte.wrapping_add(seed))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_streams_echo_large_payloads() {
    let (host, guest) = duplex(64 * 1024);
    let server = tokio::spawn(contr_mux::serve(guest, echo));
    let (opener, driver) = contr_mux::connect(host);
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
            .expect("transfer finishes in time")
            .expect("transfer succeeds");
    }

    drop(opener);
    timeout(LIMIT, driver)
        .await
        .expect("driver closes")
        .expect("driver task")
        .expect("clean close");
    timeout(LIMIT, server)
        .await
        .expect("server sees the close")
        .expect("server task")
        .expect("clean end");
}

#[tokio::test]
async fn opening_after_the_guest_is_gone_fails() {
    let (host, guest) = duplex(1024);
    drop(guest);
    let (opener, driver) = contr_mux::connect(host);
    let driver = tokio::spawn(driver);

    let result = timeout(LIMIT, opener.open(Channel::Engine))
        .await
        .expect("open resolves");
    assert!(result.is_err());
    let _ = timeout(LIMIT, driver).await.expect("driver ends");
}
