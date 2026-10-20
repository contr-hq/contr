//! Typed daemon settings, persisted per user as one JSON object.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Settings the daemon keeps across restarts. Keys this release does not know survive a rewrite, so a downgrade
/// loses nothing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// WSL distributions of the user that reach the engine.
    #[serde(default)]
    pub wsl_integrations: BTreeSet<String>,
    #[serde(flatten)]
    unknown: Map<String, Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("cannot read {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} does not hold valid settings", path.display())]
    Invalid {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("cannot write {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// The settings file of one user; its owner serializes updates.
#[derive(Clone, Debug)]
pub struct SettingsFile {
    path: PathBuf,
}

impl SettingsFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Current settings; defaults when the file does not exist.
    pub fn load(&self) -> Result<Settings, SettingsError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(source) => {
                return Err(SettingsError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };

        serde_json::from_str(&text).map_err(|source| SettingsError::Invalid {
            path: self.path.clone(),
            source,
        })
    }

    /// Replaces the file through a staging file, so a reader never sees it half-written.
    pub fn save(&self, settings: &Settings) -> Result<(), SettingsError> {
        let write = |source| SettingsError::Write {
            path: self.path.clone(),
            source,
        };
        let text = serde_json::to_string_pretty(settings)
            .map_err(io::Error::from)
            .map_err(write)?
            + "\n";
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(write)?;
        }
        let staging = self.path.with_extension("json.tmp");
        fs::write(&staging, text).map_err(write)?;

        fs::rename(&staging, &self.path).map_err(write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_holds_the_defaults() {
        let dir = tempfile::tempdir().unwrap();

        let settings = SettingsFile::new(dir.path().join("settings.json")).load().unwrap();

        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn saves_and_keeps_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"futureKey": {"on": true}, "wslIntegrations": ["Debian"]}"#).unwrap();
        let file = SettingsFile::new(path.clone());

        let mut settings = file.load().unwrap();
        settings.wsl_integrations.insert("Ubuntu".to_owned());
        file.save(&settings).unwrap();

        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["wslIntegrations"], serde_json::json!(["Debian", "Ubuntu"]));
        assert_eq!(written["futureKey"]["on"], true);
        assert_eq!(file.load().unwrap(), settings);
    }

    #[test]
    fn reports_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"wslIntegrations": "Ubuntu"}"#).unwrap();

        let error = SettingsFile::new(path).load().unwrap_err();

        assert!(matches!(error, SettingsError::Invalid { .. }));
    }
}
