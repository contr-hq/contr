//! Backend that manages a Docker Engine or Podman installed on the Linux host through its systemd units.

#![cfg(target_os = "linux")]

mod backend;
mod discovery;
mod remediate;
mod systemd;

pub use backend::HostBackend;
pub use discovery::{Bus, Candidate, Flavor, candidates};
