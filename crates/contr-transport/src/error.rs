use std::io;
use std::path::PathBuf;

/// Failure to serve or reach the daemon's API.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// No endpoint was given and the per-user runtime directory is unknown.
    #[error("cannot locate the daemon: the per-user runtime directory variable is not set")]
    NoRuntimeDir,
    /// An endpoint string is neither `unix://PATH` nor `npipe://PATH`.
    #[error("invalid endpoint {0:?}; expected unix://PATH or npipe://PATH")]
    InvalidEndpoint(String),
    /// The endpoint kind does not exist on this platform.
    #[error("{0} endpoints are not available on this platform")]
    Unsupported(&'static str),
    /// A live daemon already listens on the socket.
    #[error("another daemon already serves {}", .0.display())]
    AlreadyServing(PathBuf),
    /// Something other than a socket occupies the socket path.
    #[error("{} exists and is not a socket", .0.display())]
    NotASocket(PathBuf),
    /// The socket directory could be read or entered by another user.
    #[error("{} is not a private directory: {reason}", path.display())]
    InsecureDirectory {
        /// Directory that failed the check.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// The discovery file is not valid JSON of the expected shape.
    #[error("invalid discovery file {}", path.display())]
    Discovery {
        /// File that failed to parse.
        path: PathBuf,
        /// Parser error.
        #[source]
        source: serde_json::Error,
    },
    /// An operating-system call failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The gRPC channel could not connect.
    #[error(transparent)]
    Connect(#[from] tonic::transport::Error),
}
