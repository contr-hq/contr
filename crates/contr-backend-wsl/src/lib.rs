//! Backend that runs the engine in Contr's own WSL 2 distributions; see docs/adr/0003-wsl-two-distributions.md.
#![cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "only Windows composes the backend; its parts build and test everywhere"
    )
)]

#[cfg(windows)]
mod backend;
mod host_tcp;
mod images;
mod integration;
mod output;
mod ping;
#[cfg(windows)]
mod session;
mod wsl;

#[cfg(windows)]
pub use backend::WslBackend;
