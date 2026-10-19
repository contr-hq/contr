use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use futures_util::Stream;
use nix::unistd::geteuid;
use tokio::net::{UnixListener, UnixStream};

use crate::{Endpoint, TransportError};

const DIRECTORY_MODE: u32 = 0o700;
const SOCKET_MODE: u32 = 0o600;
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Accepted connection, ready for `tonic::transport::Server::serve_with_incoming`.
pub type Connection = UnixStream;

/// Socket listener that hands out only connections from the daemon's own user.
pub struct Listener {
    inner: UnixListener,
    path: Option<PathBuf>,
    remove_on_drop: bool,
}

impl Listener {
    /// Binds `endpoint` inside a directory only the current user can enter; a socket left by a dead daemon is
    /// replaced, a live one is an error.
    pub fn bind(endpoint: &Endpoint) -> Result<Self, TransportError> {
        let Endpoint::Unix(path) = endpoint else {
            return Err(TransportError::Unsupported("named pipe"));
        };
        let directory = path
            .parent()
            .ok_or_else(|| TransportError::InvalidEndpoint(endpoint.to_string()))?;

        prepare_directory(directory)?;
        remove_stale_socket(path)?;

        let listener = std::os::unix::net::UnixListener::bind(path)?;
        fs::set_permissions(path, Permissions::from_mode(SOCKET_MODE))?;
        listener.set_nonblocking(true)?;

        Ok(Self {
            inner: UnixListener::from_std(listener)?,
            path: Some(path.clone()),
            remove_on_drop: true,
        })
    }

    /// Takes the socket systemd passed through socket activation, if any; systemd owns its path and permissions.
    pub fn from_systemd() -> Result<Option<Self>, TransportError> {
        let mut descriptors = listenfd::ListenFd::from_env();
        let Some(listener) = descriptors.take_unix_listener(0)? else {
            return Ok(None);
        };
        listener.set_nonblocking(true)?;
        let path = listener.local_addr()?.as_pathname().map(Path::to_path_buf);

        Ok(Some(Self {
            inner: UnixListener::from_std(listener)?,
            path,
            remove_on_drop: false,
        }))
    }

    /// Endpoint clients connect to; `None` for an unnamed activated socket.
    pub fn endpoint(&self) -> Option<Endpoint> {
        self.path.clone().map(Endpoint::Unix)
    }

    /// Connections from the current effective user. Others are closed before any byte is read: the daemon controls a
    /// root-equivalent engine.
    pub fn incoming(self) -> Pin<Box<dyn Stream<Item = io::Result<Connection>> + Send>> {
        Box::pin(futures_util::stream::unfold(self, |listener| async move {
            let connection = listener.accept_same_user().await;
            Some((Ok(connection), listener))
        }))
    }

    async fn accept_same_user(&self) -> Connection {
        let own_uid = geteuid().as_raw();

        loop {
            match self.inner.accept().await {
                Ok((stream, _)) => match stream.peer_cred() {
                    Ok(peer) if peer.uid() == own_uid => return stream,
                    Ok(peer) => tracing::warn!(uid = peer.uid(), "rejected a connection from another user"),
                    Err(error) => tracing::warn!(%error, "rejected a connection without peer credentials"),
                },
                Err(error) => {
                    tracing::warn!(%error, "accepting a connection failed");
                    tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                }
            }
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(path) = self.path.as_ref().filter(|_| self.remove_on_drop) {
            let _ = fs::remove_file(path);
        }
    }
}

fn prepare_directory(directory: &Path) -> Result<(), TransportError> {
    DirBuilder::new()
        .recursive(true)
        .mode(DIRECTORY_MODE)
        .create(directory)?;

    let insecure = |reason: String| TransportError::InsecureDirectory {
        path: directory.to_path_buf(),
        reason,
    };
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.is_dir() {
        return Err(insecure("not a directory".to_owned()));
    }
    if metadata.uid() != geteuid().as_raw() {
        return Err(insecure(format!("owned by uid {}", metadata.uid())));
    }
    if metadata.mode() & 0o077 != 0 {
        fs::set_permissions(directory, Permissions::from_mode(DIRECTORY_MODE))?;
    }

    Ok(())
}

fn remove_stale_socket(path: &Path) -> Result<(), TransportError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_socket() {
        return Err(TransportError::NotASocket(path.to_path_buf()));
    }
    if StdUnixStream::connect(path).is_ok() {
        return Err(TransportError::AlreadyServing(path.to_path_buf()));
    }

    fs::remove_file(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use super::*;

    fn socket_in(directory: &Path) -> Endpoint {
        Endpoint::Unix(directory.join("contr").join("contrd.sock"))
    }

    #[tokio::test]
    async fn creates_a_private_directory_and_socket() {
        let runtime = tempfile::tempdir().unwrap();
        let endpoint = socket_in(runtime.path());
        let Endpoint::Unix(path) = &endpoint else {
            unreachable!()
        };

        let listener = Listener::bind(&endpoint).unwrap();

        let directory_mode = fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777;
        let socket_mode = fs::metadata(path).unwrap().mode() & 0o777;
        assert_eq!(directory_mode, DIRECTORY_MODE);
        assert_eq!(socket_mode, SOCKET_MODE);

        drop(listener);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn replaces_a_stale_socket_and_refuses_a_live_one() {
        let runtime = tempfile::tempdir().unwrap();
        let endpoint = socket_in(runtime.path());
        let Endpoint::Unix(path) = &endpoint else {
            unreachable!()
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(path).unwrap());

        let listener = Listener::bind(&endpoint).unwrap();

        assert!(matches!(
            Listener::bind(&endpoint),
            Err(TransportError::AlreadyServing(_))
        ));
        drop(listener);
    }

    #[tokio::test]
    async fn refuses_to_replace_a_regular_file() {
        let runtime = tempfile::tempdir().unwrap();
        let endpoint = socket_in(runtime.path());
        let Endpoint::Unix(path) = &endpoint else {
            unreachable!()
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"not a socket").unwrap();

        assert!(matches!(Listener::bind(&endpoint), Err(TransportError::NotASocket(_))));
    }

    #[tokio::test]
    async fn accepts_connections_from_the_same_user() {
        let runtime = tempfile::tempdir().unwrap();
        let endpoint = socket_in(runtime.path());
        let Endpoint::Unix(path) = endpoint.clone() else {
            unreachable!()
        };
        let mut incoming = Listener::bind(&endpoint).unwrap().incoming();

        let _client = UnixStream::connect(&path).await.unwrap();

        assert!(incoming.next().await.unwrap().is_ok());
    }
}
