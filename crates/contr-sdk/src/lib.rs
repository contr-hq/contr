//! Rust SDK for the Contr daemon API: endpoint resolution, connection and handshake, starting the daemon on demand,
//! typed errors and exec sessions. The generated messages and service clients are re-exported as [`api`].

#![warn(missing_docs)]

mod client;
mod error;
mod exec;
mod start;

pub use client::{Client, ClientIdentity};
pub use contr_api as api;
pub use contr_core::ErrorReason;
pub use contr_transport::Endpoint;
pub use error::Error;
pub use exec::{ExecEvent, ExecInput, ExecSession};
