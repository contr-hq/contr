//! Docker CLI tools that `contrd` installs for the current user from the bundle shipped beside it. A bundle and an
//! installation share one layout: `bin/` for executables, `cli-plugins/` for Docker CLI plugins, and the manifest.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Manifest of a bundle or an installation: a JSON object mapping each tool name to its version. The bundle's
/// Dockerfile writes it under this name.
pub const MANIFEST_FILE: &str = "cli.json";

const BIN_DIR: &str = "bin";
const PLUGINS_DIR: &str = "cli-plugins";
const DOCKER_CONFIG_FILE: &str = "config.json";
const DOCKER_CONFIG_BACKUP: &str = "config.json.contr-backup";

/// Tool name to version.
pub type Versions = BTreeMap<String, String>;

/// Tools bundled with the daemon.
#[derive(Clone, Debug)]
pub struct Bundle {
    dir: PathBuf,
    versions: Versions,
}

impl Bundle {
    /// Reads the bundle in `dir`.
    pub fn load(dir: &Path) -> io::Result<Self> {
        Ok(Self {
            versions: read_versions(&dir.join(MANIFEST_FILE))?,
            dir: dir.to_path_buf(),
        })
    }

    pub fn versions(&self) -> &Versions {
        &self.versions
    }
}

/// Per-user installation of the tools.
#[derive(Clone, Debug)]
pub struct Installation {
    dir: PathBuf,
}

impl Installation {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.dir.join(BIN_DIR)
    }

    pub fn plugins_dir(&self) -> PathBuf {
        self.dir.join(PLUGINS_DIR)
    }

    /// Installed versions; empty when nothing is installed.
    pub fn versions(&self) -> Versions {
        read_versions(&self.dir.join(MANIFEST_FILE)).unwrap_or_default()
    }

    /// Copies the bundle's executables, then its manifest, which marks the installation complete. Fails while one
    /// of the installed executables is running.
    pub fn install_from(&self, bundle: &Bundle) -> io::Result<()> {
        copy_files(&bundle.dir.join(BIN_DIR), &self.bin_dir())?;
        copy_files(&bundle.dir.join(PLUGINS_DIR), &self.plugins_dir())?;
        replace(&bundle.dir.join(MANIFEST_FILE), &self.dir.join(MANIFEST_FILE))
    }
}

/// Lists `plugins_dir` in the Docker CLI's `cliPluginsExtraDirs` and, when no credential store or helper is
/// configured, selects `credential_store`. The first original `config.json` is kept beside it. A `config.json` that
/// is not a JSON object is left untouched and reported as invalid.
pub fn register_with_docker(docker_config_dir: &Path, plugins_dir: &Path, credential_store: &str) -> io::Result<()> {
    let path = docker_config_dir.join(DOCKER_CONFIG_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => "{}".to_owned(),
        Err(error) => return Err(error),
    };
    let Ok(Value::Object(mut config)) = serde_json::from_str(&text) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a JSON object", path.display()),
        ));
    };

    if !merge_docker_config(&mut config, &plugins_dir.to_string_lossy(), credential_store) {
        return Ok(());
    }
    fs::create_dir_all(docker_config_dir)?;
    let backup = docker_config_dir.join(DOCKER_CONFIG_BACKUP);
    if path.exists() && !backup.exists() {
        fs::copy(&path, &backup)?;
    }
    let staging = path.with_extension("json.contr-tmp");
    fs::write(&staging, serde_json::to_string_pretty(&Value::Object(config))? + "\n")?;
    fs::rename(&staging, &path)
}

fn merge_docker_config(config: &mut Map<String, Value>, plugins_dir: &str, credential_store: &str) -> bool {
    let mut changed = false;

    let dirs = config
        .entry("cliPluginsExtraDirs")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Value::Array(dirs) = dirs
        && !dirs.iter().any(|dir| dir.as_str() == Some(plugins_dir))
    {
        dirs.push(Value::String(plugins_dir.to_owned()));
        changed = true;
    }

    if !config.contains_key("credsStore") && !config.contains_key("credHelpers") {
        config.insert("credsStore".to_owned(), Value::String(credential_store.to_owned()));
        changed = true;
    }

    changed
}

fn read_versions(path: &Path) -> io::Result<Versions> {
    let text = fs::read_to_string(path)?;

    serde_json::from_str(&text).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid {}: {error}", path.display()),
        )
    })
}

fn copy_files(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            replace(&entry.path(), &to.join(entry.file_name()))?;
        }
    }

    Ok(())
}

/// Copies `from` over `to` through a staging file, so `to` is never left half-written.
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    let mut staging = to.as_os_str().to_owned();
    staging.push(".contr-new");
    fs::copy(from, &staging)?;
    fs::rename(&staging, to)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn config(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => unreachable!("tests pass objects"),
        }
    }

    #[test]
    fn adds_the_plugins_dir_and_a_credential_store() {
        let mut existing = config(json!({"auths": {}, "cliPluginsExtraDirs": ["D:/other"]}));

        assert!(merge_docker_config(&mut existing, "C:/contr/cli-plugins", "wincred"));

        assert_eq!(
            existing["cliPluginsExtraDirs"],
            json!(["D:/other", "C:/contr/cli-plugins"])
        );
        assert_eq!(existing["credsStore"], "wincred");
        assert_eq!(existing["auths"], json!({}));
    }

    #[test]
    fn keeps_existing_credential_settings_and_is_idempotent() {
        let mut existing = config(json!({"credHelpers": {"gcr.io": "gcloud"}}));

        assert!(merge_docker_config(&mut existing, "C:/contr/cli-plugins", "wincred"));
        assert!(!existing.contains_key("credsStore"));
        assert!(!merge_docker_config(&mut existing, "C:/contr/cli-plugins", "wincred"));
    }

    #[test]
    fn installs_a_bundle_and_registers_it_with_docker() {
        let root = tempfile::tempdir().unwrap();
        let bundle_dir = root.path().join("bundle");
        fs::create_dir_all(bundle_dir.join(BIN_DIR)).unwrap();
        fs::create_dir_all(bundle_dir.join(PLUGINS_DIR)).unwrap();
        fs::write(bundle_dir.join(BIN_DIR).join("docker.exe"), "docker").unwrap();
        fs::write(bundle_dir.join(PLUGINS_DIR).join("docker-compose.exe"), "compose").unwrap();
        fs::write(
            bundle_dir.join(MANIFEST_FILE),
            r#"{"docker": "29.9.1", "compose": "5.6.0"}"#,
        )
        .unwrap();
        let docker_config = root.path().join(".docker");
        fs::create_dir_all(&docker_config).unwrap();
        fs::write(docker_config.join(DOCKER_CONFIG_FILE), r#"{"currentContext": "contr"}"#).unwrap();

        let bundle = Bundle::load(&bundle_dir).unwrap();
        let installation = Installation::new(root.path().join("installed"));
        assert!(installation.versions().is_empty());
        installation.install_from(&bundle).unwrap();
        register_with_docker(&docker_config, &installation.plugins_dir(), "wincred").unwrap();

        assert_eq!(&installation.versions(), bundle.versions());
        assert_eq!(
            fs::read_to_string(installation.bin_dir().join("docker.exe")).unwrap(),
            "docker"
        );
        let written: Value =
            serde_json::from_str(&fs::read_to_string(docker_config.join(DOCKER_CONFIG_FILE)).unwrap()).unwrap();
        assert_eq!(written["currentContext"], "contr");
        assert_eq!(written["credsStore"], "wincred");
        assert!(docker_config.join(DOCKER_CONFIG_BACKUP).exists());
    }

    #[test]
    fn refuses_a_config_that_is_not_an_object() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(DOCKER_CONFIG_FILE), "[]").unwrap();

        let error = register_with_docker(root.path(), Path::new("C:/x"), "wincred").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read_to_string(root.path().join(DOCKER_CONFIG_FILE)).unwrap(), "[]");
    }
}
