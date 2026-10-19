//! Rust SDK target: prost messages and tonic stubs, client and server gated behind the `client` and `server`
//! features of the consuming crate.

use std::fs;
use std::io;
use std::path::Path;

use crate::{Contract, Error};

const INCLUDE_FILE: &str = "mod.rs";
const REVISION_SOURCE: &str = "revision.rs";

/// Replaces the generated sources in `out_dir` with code for `contract`.
pub fn generate(contract: &Contract, out_dir: &Path) -> Result<(), Error> {
    let write_error = |source| Error::Write {
        path: out_dir.to_path_buf(),
        source,
    };

    fs::create_dir_all(out_dir).map_err(write_error)?;
    remove_generated(out_dir).map_err(write_error)?;

    tonic_prost_build::configure()
        .build_transport(false)
        .client_mod_attribute(".", r#"#[cfg(feature = "client")]"#)
        .server_mod_attribute(".", r#"#[cfg(feature = "server")]"#)
        .include_file(INCLUDE_FILE)
        .out_dir(out_dir)
        .compile_fds(contract.descriptors.clone())
        .map_err(write_error)?;

    let revision = format!(
        "/// Value of `proto/API_REVISION` this code was generated from.\npub const API_REVISION: u32 = {};\n",
        contract.revision
    );
    fs::write(out_dir.join(REVISION_SOURCE), revision).map_err(write_error)
}

/// Deletes only files this generator owns, so a mistyped `out_dir` cannot lose hand-written sources.
fn remove_generated(out_dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(out_dir)? {
        let path = entry?.path();
        if is_generated(&path) {
            fs::remove_file(&path)?;
        }
    }

    Ok(())
}

fn is_generated(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let is_rust = path.extension().is_some_and(|extension| extension == "rs");

    name == INCLUDE_FILE || name == REVISION_SOURCE || (is_rust && name.starts_with("contr."))
}
