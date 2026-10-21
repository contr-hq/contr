//! Product identity, error reasons and remediations shared by every Contr crate.

#![warn(missing_docs)]

mod error;
pub mod guest;
pub mod kube;
pub mod paths;
pub mod product;
pub mod proxy;

pub use error::{ErrorReason, Remediation};
