use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, ensure};
use contr_core::guest::EngineConfig;

use crate::engine::DOCKERD;

/// Directory of what the engine uses besides the guest's defaults.
const CONFIG_DIR: &str = "/etc/contr/engine";
/// Certificates the engine trusts besides the guest's own; `dockerd` finds them through `SSL_CERT_DIR`.
const CERTS_DIR: &str = "certs";
const HOST_CERTIFICATES: &str = "host.pem";

/// The engine's `daemon.json`, once `configure` wrote one.
pub(crate) fn daemon_file() -> Option<PathBuf> {
    Some(Path::new(CONFIG_DIR).join("daemon.json")).filter(|path| path.is_file())
}

/// Directory whose certificates the engine trusts besides the guest's own.
pub(crate) fn certificates_dir() -> PathBuf {
    Path::new(CONFIG_DIR).join(CERTS_DIR)
}

/// Writes the [`EngineConfig`] on standard input for the engine's next start, once `dockerd` accepts it.
pub(crate) fn configure() -> anyhow::Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let config: EngineConfig = serde_json::from_str(&input).context("standard input holds no engine configuration")?;
    write(Path::new(CONFIG_DIR), &config, validate)
}

fn write(dir: &Path, config: &EngineConfig, validate: impl Fn(&Path) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let certificates = dir.join(CERTS_DIR);
    fs::create_dir_all(&certificates).with_context(|| format!("cannot create {}", certificates.display()))?;

    let daemon = dir.join("daemon.json");
    let staged = daemon.with_extension("json.new");
    fs::write(&staged, serde_json::to_vec_pretty(&config.daemon)?)
        .with_context(|| format!("cannot write {}", staged.display()))?;
    validate(&staged)?;
    fs::rename(&staged, &daemon).with_context(|| format!("cannot replace {}", daemon.display()))?;

    let bundle = certificates.join(HOST_CERTIFICATES);
    if config.certificates.is_empty() {
        match fs::remove_file(&bundle) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(error).with_context(|| format!("cannot remove {}", bundle.display()));
            }
            _ => {}
        }
    } else {
        let staged = bundle.with_extension("pem.new");
        fs::write(&staged, &config.certificates).with_context(|| format!("cannot write {}", staged.display()))?;
        fs::rename(&staged, &bundle).with_context(|| format!("cannot replace {}", bundle.display()))?;
    }

    Ok(())
}

/// Fails with `dockerd`'s explanation when it rejects the configuration file at `path`.
fn validate(path: &Path) -> anyhow::Result<()> {
    let output = Command::new(DOCKERD)
        .arg("--validate")
        .arg("--config-file")
        .arg(path)
        .output()
        .context("cannot run dockerd")?;
    ensure!(
        output.status.success(),
        "the engine rejects its configuration: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn config(daemon: &serde_json::Value, certificates: &str) -> EngineConfig {
        EngineConfig {
            daemon: daemon.as_object().cloned().unwrap_or_default(),
            certificates: certificates.to_owned(),
        }
    }

    #[test]
    fn writes_the_configuration_and_removes_certificates_not_given() {
        let dir = tempfile::tempdir().unwrap();
        let accepted = |_: &Path| Ok(());

        write(
            dir.path(),
            &config(&json!({"registry-mirrors": ["https://m"]}), "PEM"),
            accepted,
        )
        .unwrap();
        let daemon: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join("daemon.json")).unwrap()).unwrap();
        assert_eq!(daemon, json!({"registry-mirrors": ["https://m"]}));
        assert_eq!(fs::read_to_string(dir.path().join("certs/host.pem")).unwrap(), "PEM");

        write(dir.path(), &config(&json!({}), ""), accepted).unwrap();
        assert!(!dir.path().join("certs/host.pem").exists());
        assert!(dir.path().join("certs").is_dir());
    }

    #[test]
    fn keeps_the_previous_configuration_when_the_engine_rejects_the_new_one() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &config(&json!({"debug": true}), ""), |_| Ok(())).unwrap();

        let error = write(dir.path(), &config(&json!({"nope": 1}), ""), |_| {
            anyhow::bail!("unknown key")
        })
        .unwrap_err();

        assert_eq!(error.to_string(), "unknown key");
        let daemon: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join("daemon.json")).unwrap()).unwrap();
        assert_eq!(daemon, json!({"debug": true}));
    }
}
