use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::TransportError;

/// Discovery file the daemon writes into its runtime directory. Every SDK reads it, so renaming a field breaks deployed
/// clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryFile {
    /// Daemon process ID.
    pub pid: u32,
    /// Daemon version.
    pub version: String,
    /// `proto/API_REVISION` the daemon serves.
    pub api_revision: u32,
    /// Endpoints in [`crate::Endpoint`] string form, preferred first.
    pub endpoints: Vec<String>,
}

impl DiscoveryFile {
    /// Reads and parses `path`.
    pub fn read(path: &Path) -> Result<Self, TransportError> {
        let text = fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(|source| TransportError::Discovery {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Replaces `path` atomically with a file only its owner can read.
    pub fn write(&self, path: &Path) -> Result<(), TransportError> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let staging = staging_path(path);

        let mut file = private_file(&staging)?;
        file.write_all(&json)?;
        file.sync_all()?;
        fs::rename(&staging, path)?;

        Ok(())
    }

    /// Deletes `path` when it still names process `pid`, so a newer daemon's file survives.
    pub fn remove(path: &Path, pid: u32) {
        if Self::read(path).is_ok_and(|file| file.pid == pid) {
            let _ = fs::remove_file(path);
        }
    }
}

fn staging_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

#[cfg(unix)]
fn private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
fn private_file(path: &Path) -> io::Result<fs::File> {
    fs::File::create(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_removes_only_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        let file = DiscoveryFile {
            pid: 42,
            version: "0.1.0".to_owned(),
            api_revision: 2,
            endpoints: vec!["unix:///run/user/1000/contr/contrd.sock".to_owned()],
        };

        file.write(&path).unwrap();
        assert_eq!(DiscoveryFile::read(&path).unwrap(), file);

        DiscoveryFile::remove(&path, 7);
        assert!(path.exists());
        DiscoveryFile::remove(&path, 42);
        assert!(!path.exists());
    }
}
