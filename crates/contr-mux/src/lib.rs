//! Stream multiplexing between `contrd` and the guest agent over one byte stream, such as the standard input and
//! output of `wsl.exe`. Either side opens streams to the other through a [`session`].

#![warn(missing_docs)]

use std::future::{Future, poll_fn};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::task::Poll;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;
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
    /// The engine's Docker API socket in the guest.
    Engine = 1,
    /// A TCP connection to the host's loopback interface; its port follows as two big-endian bytes.
    HostTcp = 2,
    /// A TCP connection to the guest's loopback interface; its port follows as two big-endian bytes.
    GuestTcp = 3,
}

impl TryFrom<u8> for Channel {
    type Error = MuxError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Engine),
            2 => Ok(Self::HostTcp),
            3 => Ok(Self::GuestTcp),
            other => Err(MuxError::UnknownChannel(other)),
        }
    }
}

/// Side of a session; only the host closes it once its last [`Opener`] is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// `contrd`.
    Host,
    /// The guest agent.
    Guest,
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

/// Opens streams to the peer of a session; clones share the session.
#[derive(Clone, Debug)]
pub struct Opener {
    requests: mpsc::Sender<Reply>,
}

impl Opener {
    /// Opens a stream to `channel` on the peer.
    pub async fn open(&self, channel: Channel) -> Result<MuxStream, MuxError> {
        let (reply, response) = oneshot::channel();
        self.requests.send(reply).await.map_err(|_| MuxError::Closed)?;
        let mut stream = response.await.map_err(|_| MuxError::Closed)??.compat();
        stream.write_all(&[channel as u8]).await?;

        Ok(stream)
    }

    /// Opens a [`Channel::HostTcp`] stream to `port` on the host's loopback interface.
    pub async fn open_host_tcp(&self, port: u16) -> Result<MuxStream, MuxError> {
        self.open_tcp(Channel::HostTcp, port).await
    }

    /// Opens a [`Channel::GuestTcp`] stream to `port` on the guest's loopback interface.
    pub async fn open_guest_tcp(&self, port: u16) -> Result<MuxStream, MuxError> {
        self.open_tcp(Channel::GuestTcp, port).await
    }

    async fn open_tcp(&self, channel: Channel, port: u16) -> Result<MuxStream, MuxError> {
        let mut stream = self.open(channel).await?;
        stream.write_all(&port.to_be_bytes()).await?;

        Ok(stream)
    }
}

/// Port named by a [`Channel::HostTcp`] or [`Channel::GuestTcp`] stream, read after its channel byte.
pub async fn read_port(stream: &mut MuxStream) -> Result<u16, MuxError> {
    let mut port = [0; 2];
    stream.read_exact(&mut port).await?;

    Ok(u16::from_be_bytes(port))
}

/// Joins a [`Channel::HostTcp`] or [`Channel::GuestTcp`] stream to the port it names on this side's loopback
/// interface, trying IPv4 before IPv6, until both directions close.
pub async fn join_loopback(mut stream: MuxStream) -> Result<(), MuxError> {
    let port = read_port(&mut stream).await?;
    let mut socket = match TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await {
        Ok(socket) => socket,
        Err(_) => TcpStream::connect((Ipv6Addr::LOCALHOST, port)).await?,
    };
    tokio::io::copy_bidirectional(&mut stream, &mut socket).await?;

    Ok(())
}

/// Starts a session over `io`. Each stream the peer opens goes to `handle` on its own task. The returned future
/// drives every stream; it resolves when the peer closes the session or, on the host, after the last [`Opener`] is
/// dropped and the session is closed.
pub fn session<T, H, F>(io: T, role: Role, handle: H) -> (Opener, impl Future<Output = Result<(), MuxError>> + Send)
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    H: Fn(Channel, MuxStream) -> F + Send + Sync + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    let handle = Arc::new(handle);
    let (requests, receiver) = mpsc::channel::<Reply>(16);
    let mode = match role {
        Role::Host => Mode::Client,
        Role::Guest => Mode::Server,
    };
    let mut connection = Connection::new(buffered(io), config(), mode);

    let driver = async move {
        let mut pending = Some(receiver);
        let mut waiting: Option<Reply> = None;
        let mut closing = false;
        poll_fn(|cx| {
            loop {
                if closing {
                    return connection.poll_close(cx).map_err(MuxError::from);
                }
                if waiting.is_none()
                    && let Some(receiver) = pending.as_mut()
                {
                    match receiver.poll_recv(cx) {
                        Poll::Ready(Some(reply)) => waiting = Some(reply),
                        Poll::Ready(None) => {
                            pending = None;
                            closing = role == Role::Host;
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
                    Poll::Ready(Some(Ok(stream))) => accept(stream, Arc::clone(&handle)),
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

fn accept<H, F>(stream: yamux::Stream, handle: Arc<H>)
where
    H: Fn(Channel, MuxStream) -> F + Send + Sync + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut stream = stream.compat();
        match read_channel(&mut stream).await {
            Ok(channel) => handle(channel, stream).await,
            Err(error) => tracing::warn!(%error, "rejected a multiplexed stream"),
        }
    });
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
        assert_eq!(Channel::try_from(Channel::HostTcp as u8).ok(), Some(Channel::HostTcp));
        assert_eq!(Channel::try_from(Channel::GuestTcp as u8).ok(), Some(Channel::GuestTcp));
        assert!(matches!(Channel::try_from(0), Err(MuxError::UnknownChannel(0))));
    }
}
