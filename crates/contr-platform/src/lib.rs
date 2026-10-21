//! Operating-system integration: directories, docker contexts, autostart, certificates and proxies.

pub mod cli_tools;
pub mod disk;
pub mod docker_context;
#[cfg(windows)]
pub mod elevation;
pub mod policy;
pub mod ports;
#[cfg(windows)]
pub mod user_path;
