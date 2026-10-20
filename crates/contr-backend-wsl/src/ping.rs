use std::io;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

const REQUEST: &[u8] = b"GET /_ping HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n";

/// Sends the Docker Engine API ping over `stream` and reports whether the engine answered 200 OK.
pub(crate) async fn engine_answers<S>(mut stream: S) -> io::Result<bool>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(REQUEST).await?;
    stream.flush().await?;

    let mut status_line = String::new();
    BufReader::new(stream).read_line(&mut status_line).await?;

    Ok(status_line.split_whitespace().nth(1) == Some("200"))
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, duplex};

    use super::*;

    async fn answer(response: &'static str) -> io::Result<bool> {
        let (client, mut engine) = duplex(4096);
        let engine = tokio::spawn(async move {
            let mut request = vec![0; REQUEST.len()];
            engine.read_exact(&mut request).await.expect("request arrives");
            assert_eq!(request, REQUEST);
            engine.write_all(response.as_bytes()).await.expect("response is sent");
        });
        let answered = engine_answers(client).await;
        engine.await.expect("engine task");

        answered
    }

    #[tokio::test]
    async fn ok_status_means_the_engine_answers() {
        let answered = answer("HTTP/1.1 200 OK\r\nApi-Version: 1.52\r\n\r\nOK").await;

        assert!(answered.expect("ping completes"));
    }

    #[tokio::test]
    async fn other_status_means_it_does_not() {
        let answered = answer("HTTP/1.1 503 Service Unavailable\r\n\r\n").await;

        assert!(!answered.expect("ping completes"));
    }
}
