//! Local IPC for the Contr API: Unix sockets with peer-credential checks and per-user Windows named pipes. The daemon
//! never listens on TCP.

#![warn(missing_docs)]

mod client;
mod discovery;
mod endpoint;
mod error;
#[cfg(windows)]
mod pipe;
#[cfg(unix)]
mod unix;

pub use client::connect;
pub use discovery::DiscoveryFile;
pub use endpoint::Endpoint;
pub use error::TransportError;
#[cfg(windows)]
pub use pipe::{Connection, Listener, current_user_sid};
#[cfg(unix)]
pub use unix::{Connection, Listener};
