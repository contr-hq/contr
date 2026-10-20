use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use contr_core::guest;
use serde::Deserialize;

/// Guest root filesystems bundled with this release, as described by their manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GuestImages {
    pub(crate) release: String,
    pub(crate) runtime: PathBuf,
    pub(crate) data: PathBuf,
}

#[derive(Deserialize)]
struct Manifest {
    release: String,
    runtime: String,
    data: String,
}

impl GuestImages {
    /// Reads the manifest in `dir`; the images it names must exist next to it.
    pub(crate) fn load(dir: &Path) -> io::Result<Self> {
        let path = dir.join(guest::MANIFEST_FILE);
        let text = fs::read_to_string(&path)
            .map_err(|error| io::Error::new(error.kind(), format!("cannot read {}: {error}", path.display())))?;
        let manifest: Manifest = serde_json::from_str(&text).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid {}: {error}", path.display()),
            )
        })?;

        let images = Self {
            release: manifest.release,
            runtime: dir.join(manifest.runtime),
            data: dir.join(manifest.data),
        };
        for image in [&images.runtime, &images.data] {
            if !image.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("guest image {} is missing", image.display()),
                ));
            }
        }

        Ok(images)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) {
        fs::write(dir.join(name), contents).expect("test file is written");
    }

    #[test]
    fn loads_a_complete_bundle() {
        let dir = tempfile::tempdir().expect("temporary directory");
        write(dir.path(), "runtime.tar", "");
        write(dir.path(), "data.tar", "");
        write(
            dir.path(),
            guest::MANIFEST_FILE,
            r#"{"release": "0.1.0+abc", "runtime": "runtime.tar", "data": "data.tar"}"#,
        );

        let images = GuestImages::load(dir.path()).expect("bundle loads");

        assert_eq!(images.release, "0.1.0+abc");
        assert_eq!(images.runtime, dir.path().join("runtime.tar"));
        assert_eq!(images.data, dir.path().join("data.tar"));
    }

    #[test]
    fn rejects_a_manifest_naming_a_missing_image() {
        let dir = tempfile::tempdir().expect("temporary directory");
        write(dir.path(), "runtime.tar", "");
        write(
            dir.path(),
            guest::MANIFEST_FILE,
            r#"{"release": "0.1.0", "runtime": "runtime.tar", "data": "data.tar"}"#,
        );

        let error = GuestImages::load(dir.path()).expect_err("bundle is incomplete");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
