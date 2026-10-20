//! Docker CLI contexts, stored the way the Docker CLI stores them (docker/cli `cli/context/store`, Apache-2.0):
//! `<config>/contexts/meta/<sha256 of the name>/meta.json`.

use std::fmt::Write;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// What [`ensure`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Created,
    Updated,
    Unchanged,
}

/// Docker CLI configuration directory: `DOCKER_CONFIG`, else `.docker` in the home directory.
pub fn config_dir() -> Option<PathBuf> {
    std::env::var_os("DOCKER_CONFIG")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".docker")))
}

/// The context the Docker CLI uses when neither `--context`, `DOCKER_CONTEXT` nor `DOCKER_HOST` names one.
pub fn current(config_dir: &Path) -> io::Result<String> {
    let config = match fs::read(config_dir.join("config.json")) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).map_err(io::Error::other)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Value::Null,
        Err(error) => return Err(error),
    };

    Ok(config["currentContext"]
        .as_str()
        .filter(|name| !name.is_empty())
        .unwrap_or("default")
        .to_owned())
}

/// Points context `name` at `host`, creating it when missing. Other endpoints, TLS material and the current-context
/// setting are left as they are.
pub fn ensure(config_dir: &Path, name: &str, description: &str, host: &str) -> io::Result<Outcome> {
    let meta_dir = config_dir.join("contexts").join("meta").join(digest(name));
    let meta_file = meta_dir.join("meta.json");

    let existing = match fs::read(&meta_file) {
        Ok(bytes) => Some(serde_json::from_slice::<Value>(&bytes).map_err(io::Error::other)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let outcome = match &existing {
        Some(meta) if meta["Endpoints"]["docker"]["Host"] == host => return Ok(Outcome::Unchanged),
        Some(_) => Outcome::Updated,
        None => Outcome::Created,
    };

    let mut meta = existing.unwrap_or_else(|| json!({ "Metadata": { "Description": description }, "Endpoints": {} }));
    meta["Name"] = json!(name);
    meta["Endpoints"]["docker"]["Host"] = json!(host);
    if meta["Endpoints"]["docker"].get("SkipTLSVerify").is_none() {
        meta["Endpoints"]["docker"]["SkipTLSVerify"] = json!(false);
    }

    fs::create_dir_all(&meta_dir)?;
    let staging = meta_dir.join(format!("meta.json.{}.tmp", std::process::id()));
    fs::write(&staging, serde_json::to_vec(&meta).map_err(io::Error::other)?)?;
    fs::rename(&staging, &meta_file)?;

    Ok(outcome)
}

fn digest(name: &str) -> String {
    Sha256::digest(name.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(config: &Path, name: &str) -> Value {
        let file = config.join("contexts/meta").join(digest(name)).join("meta.json");
        serde_json::from_slice(&fs::read(file).unwrap()).unwrap()
    }

    #[test]
    fn names_the_directory_after_the_sha256_of_the_context() {
        assert_eq!(
            digest("default"),
            "37a8eec1ce19687d132fe29051dca629d164e2c4958ba141d5f4133a33f0688f"
        );
    }

    #[test]
    fn creates_then_updates_only_the_docker_host() {
        let config = tempfile::tempdir().unwrap();

        let created = ensure(config.path(), "contr", "Contr", "unix:///var/run/docker.sock").unwrap();
        let unchanged = ensure(config.path(), "contr", "Contr", "unix:///var/run/docker.sock").unwrap();
        let mut meta = read(config.path(), "contr");
        meta["Endpoints"]["docker"]["SkipTLSVerify"] = json!(true);
        meta["Metadata"]["Description"] = json!("edited");
        let file = config
            .path()
            .join("contexts/meta")
            .join(digest("contr"))
            .join("meta.json");
        fs::write(file, meta.to_string()).unwrap();
        let updated = ensure(config.path(), "contr", "Contr", "unix:///run/user/1000/docker.sock").unwrap();

        assert_eq!(
            (created, unchanged, updated),
            (Outcome::Created, Outcome::Unchanged, Outcome::Updated)
        );
        let meta = read(config.path(), "contr");
        assert_eq!(meta["Name"], "contr");
        assert_eq!(meta["Endpoints"]["docker"]["Host"], "unix:///run/user/1000/docker.sock");
        assert_eq!(meta["Endpoints"]["docker"]["SkipTLSVerify"], true);
        assert_eq!(meta["Metadata"]["Description"], "edited");
    }
}
