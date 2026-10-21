//! Redacted diagnostics bundles and the daemon's recent log.

mod bundle;
mod log_ring;
mod redact;

pub use bundle::{Bundle, BundleError};
pub use log_ring::LogRing;
pub use redact::{Identity, Redactor};
