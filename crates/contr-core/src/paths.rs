//! Per-user locations that the daemon and every client derive the same way.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::product;

const SETTINGS_FILE: &str = "settings.json";

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

/// File holding the daemon's settings: `$XDG_CONFIG_HOME/contr/settings.json` (or under `~/.config`) on Unix and
/// `%LOCALAPPDATA%\Contr\settings.json` on Windows; `None` when those variables are unset.
pub fn settings_file() -> Option<PathBuf> {
    #[cfg(unix)]
    let dir = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|dir| dir.join(product::SLUG));
    #[cfg(windows)]
    let dir = env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join(product::DISPLAY_NAME));

    dir.filter(|dir| dir.is_absolute()).map(|dir| dir.join(SETTINGS_FILE))
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

/// Writes `contents` to `path`, creating or truncating it, so that on Unix only the current user can read it; on Windows
/// the file inherits the access of its directory, which for the per-user directories Contr writes to is the user's
/// alone. For files that can hold secrets, such as proxy credentials or a cluster's client key.
pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    // Opening an existing file keeps its mode, so one left by an earlier write is tightened here.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    #[test]
    fn private_files_are_the_owners_alone() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        write_private(&path, b"new").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
