//! Product identity, error reasons and remediations shared by every Contr crate.

#![warn(missing_docs)]

mod error;
pub mod product;

pub use error::{ErrorReason, Remediation};
