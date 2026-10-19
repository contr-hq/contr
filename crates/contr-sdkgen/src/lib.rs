//! Generates Contr SDKs from the proto contract.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use prost_types::FileDescriptorSet;

pub mod csharp;
pub mod rust;
pub mod verify;

pub const REVISION_FILE: &str = "API_REVISION";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot read {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} must contain a single unsigned integer", .0.display())]
    Revision(PathBuf),
    #[error(transparent)]
    Compile(#[from] Box<protox::Error>),
    #[error("cannot write generated sources to {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot run {program}")]
    Spawn {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("the daemon did not listen on {0} in time")]
    DaemonStart(String),
    #[error("{program} failed with {status}")]
    Failed {
        program: String,
        status: std::process::ExitStatus,
    },
}

/// Compiled proto contract plus the API revision it declares.
pub struct Contract {
    descriptors: FileDescriptorSet,
    revision: u32,
    root: PathBuf,
    files: Vec<PathBuf>,
}

impl Contract {
    /// Compiles every `.proto` file under `proto_root` and reads its revision file.
    pub fn load(proto_root: &Path) -> Result<Self, Error> {
        let files = proto_files(proto_root)?;
        let descriptors = protox::compile(&files, [proto_root]).map_err(Box::new)?;
        let revision = read_revision(&proto_root.join(REVISION_FILE))?;

        Ok(Self {
            descriptors,
            revision,
            root: proto_root.to_path_buf(),
            files,
        })
    }

    pub fn revision(&self) -> u32 {
        self.revision
    }

    /// Proto files relative to the proto root, sorted.
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn proto_files(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut files = Vec::new();
    collect_proto_files(root, root, &mut files)?;
    files.sort();

    Ok(files)
}

fn collect_proto_files(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), Error> {
    let read_error = |source| Error::Read {
        path: dir.to_path_buf(),
        source,
    };

    for entry in fs::read_dir(dir).map_err(read_error)? {
        let path = entry.map_err(read_error)?.path();

        if path.is_dir() {
            collect_proto_files(root, &path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "proto") {
            let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            files.push(relative);
        }
    }

    Ok(())
}

fn read_revision(path: &Path) -> Result<u32, Error> {
    let text = fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })?;

    text.trim().parse().map_err(|_| Error::Revision(path.to_path_buf()))
}
