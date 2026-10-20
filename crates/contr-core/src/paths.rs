//! Per-user locations that the daemon and every client derive the same way.

use std::env;
use std::path::PathBuf;

use crate::product;

/// Directory holding the daemon's socket and discovery file: `$XDG_RUNTIME_DIR/contr` on Unix and
/// `%LOCALAPPDATA%\Contr\run` on Windows; `None` when that variable is unset.
pub fn runtime_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    let base = env::var_os("XDG_RUNTIME_DIR").map(|dir| PathBuf::from(dir).join(product::SLUG));
    #[cfg(windows)]
    let base = env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join(product::DISPLAY_NAME).join("run"));

    base.filter(|dir| dir.is_absolute())
}

/// Directory holding the WSL distributions' virtual disks: `%LOCALAPPDATA%\Contr\wsl`; `None` when that variable
/// is unset.
#[cfg(windows)]
pub fn wsl_dir() -> Option<PathBuf> {
    env::var_os("LOCALAPPDATA")
        .map(|dir| PathBuf::from(dir).join(product::DISPLAY_NAME).join("wsl"))
        .filter(|dir| dir.is_absolute())
}

/// Directory holding the Docker CLI tools installed for the user: `%LOCALAPPDATA%\Contr\cli`; `None` when that
/// variable is unset.
#[cfg(windows)]
pub fn cli_dir() -> Option<PathBuf> {
    env::var_os("LOCALAPPDATA")
        .map(|dir| PathBuf::from(dir).join(product::DISPLAY_NAME).join("cli"))
        .filter(|dir| dir.is_absolute())
}

/// Default path of the daemon's gRPC socket.
#[cfg(unix)]
pub fn api_socket() -> Option<PathBuf> {
    runtime_dir().map(|dir| dir.join(product::API_SOCKET_FILE))
}

/// Path of the running daemon's discovery file.
pub fn discovery_file() -> Option<PathBuf> {
    runtime_dir().map(|dir| dir.join(product::DISCOVERY_FILE))
}
