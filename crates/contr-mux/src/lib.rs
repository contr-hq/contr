//! Stream multiplexing between `contrd` and the guest agent over one byte stream, such as the standard input and
//! output of `wsl.exe`. The host opens streams with [`connect`]; the guest accepts them with [`serve`].

#![warn(missing_docs)]

use std::future::{Future, poll_fn};
use std::io;
use std::sync::Arc;
use std::task::Poll;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use yamux::{Config, Connection, ConnectionError, Mode};

/// Session I/O buffer; each write to `wsl.exe`'s standard streams crosses the WSL interop boundary, so writes are
/// batched until yamux flushes.
const IO_BUFFER: usize = 256 * 1024;

/// Largest yamux data frame; at most half the default stream window, so a frame never waits for credit alone.
const FRAME_SIZE: usize = 128 * 1024;

/// Multiplexed stream; it moves data only while its session's driver future runs.
pub type MuxStream = Compat<yamux::Stream>;

/// Service a stream reaches in the guest. Its value is the first byte of every stream, so renumbering a variant
/// breaks guests built by other releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum Channel {
    /// The engine's Docker API socket.
    Engine = 1,
}

impl TryFrom<u8> for Channel {
    type Error = MuxError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Engine),
            other => Err(MuxError::UnknownChannel(other)),
        }
    }
}

/// Failure of a multiplexed session or stream.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    /// The session has ended.
    #[error("the multiplexed session is closed")]
    Closed,
    /// The session failed.
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    /// A stream failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A stream named a channel this build does not know.
    #[error("unknown channel {0}")]
    UnknownChannel(u8),
}

type Reply = oneshot::Sender<Result<yamux::Stream, ConnectionError>>;

/// Opens streams on the host side of a session; clones share the session.
#[derive(Clone, Debug)]
pub struct Opener {
    requests: mpsc::Sender<Reply>,
}

impl Opener {
    /// Opens a stream to `channel` in the guest.
    pub async fn open(&self, channel: Channel) -> Result<MuxStream, MuxError> {
        let (reply, response) = oneshot::channel();
        self.requests.send(reply).await.map_err(|_| MuxError::Closed)?;
        let mut stream = response.await.map_err(|_| MuxError::Closed)??.compat();
        stream.write_all(&[channel as u8]).await?;

        Ok(stream)
    }
}

/// Starts the host side of a session over `io`. The returned future drives every stream; it resolves when the guest
/// closes the session, or after the last [`Opener`] is dropped and the session is closed.
pub fn connect<T>(io: T) -> (Opener, impl Future<Output = Result<(), MuxError>> + Send)
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (requests, mut pending) = mpsc::channel::<Reply>(16);
    let mut connection = Connection::new(buffered(io), config(), Mode::Client);

    let driver = async move {
        let mut waiting: Option<Reply> = None;
        let mut closing = false;
        poll_fn(|cx| {
            loop {
                if closing {
                    return connection.poll_close(cx).map_err(MuxError::from);
                }
                if waiting.is_none() {
                    match pending.poll_recv(cx) {
                        Poll::Ready(Some(reply)) => waiting = Some(reply),
                        Poll::Ready(None) => {
                            closing = true;
                            continue;
                        }
                        Poll::Pending => {}
                    }
                }
                if let Some(reply) = waiting.take() {
                    match connection.poll_new_outbound(cx) {
                        Poll::Ready(result) => {
                            let _ = reply.send(result);
                            continue;
                        }
                        Poll::Pending => waiting = Some(reply),
                    }
                }
                match connection.poll_next_inbound(cx) {
                    Poll::Ready(Some(Ok(_unsolicited))) => {}
                    Poll::Ready(Some(Err(error))) => return Poll::Ready(Err(error.into())),
                    Poll::Ready(None) => return Poll::Ready(Ok(())),
                    Poll::Pending => return Poll::Pending,
                }
            }
        })
        .await
    };

    (Opener { requests }, driver)
}

/// Runs the guest side of a session over `io`, handing each stream to `handle` on its own task. Resolves when the
/// host closes the session.
pub async fn serve<T, H, F>(io: T, handle: H) -> Result<(), MuxError>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    H: Fn(Channel, MuxStream) -> F + Send + Sync + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    let handle = Arc::new(handle);
    let mut connection = Connection::new(buffered(io), config(), Mode::Server);

    loop {
        let stream = match poll_fn(|cx| connection.poll_next_inbound(cx)).await {
            Some(Ok(stream)) => stream,
            Some(Err(error)) => return Err(error.into()),
            None => return Ok(()),
        };
        let handle = Arc::clone(&handle);
        tokio::spawn(async move {
            let mut stream = stream.compat();
            match read_channel(&mut stream).await {
                Ok(channel) => handle(channel, stream).await,
                Err(error) => tracing::warn!(%error, "rejected a multiplexed stream"),
            }
        });
    }
}

fn config() -> Config {
    let mut config = Config::default();
    config.set_split_send_size(FRAME_SIZE);
    config
}

fn buffered<T>(io: T) -> Compat<BufStream<T>>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    BufStream::with_capacity(IO_BUFFER, IO_BUFFER, io).compat()
}

async fn read_channel(stream: &mut MuxStream) -> Result<Channel, MuxError> {
    let mut byte = [0];
    stream.read_exact(&mut byte).await?;

    Channel::try_from(byte[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_values_round_trip() {
        assert_eq!(Channel::try_from(Channel::Engine as u8).ok(), Some(Channel::Engine));
        assert!(matches!(Channel::try_from(0), Err(MuxError::UnknownChannel(0))));
    }
}
