//! The user's kubeconfig file, where the cluster's context lives beside the user's own.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlEmitter, YamlLoader};

/// Suffix of the copy of the kubeconfig made before Contr changes it.
const BACKUP_SUFFIX: &str = "contr-backup";

#[derive(Debug, thiserror::Error)]
pub enum KubeconfigError {
    #[error("cannot read {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot write {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is not a kubeconfig: {reason}", path.display())]
    Invalid { path: PathBuf, reason: String },
    #[error("the cluster's kubeconfig lacks {0}")]
    Incomplete(&'static str),
}

/// What kubectl needs to reach the cluster, as k3s writes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Access {
    pub server: String,
    pub certificate_authority_data: String,
    pub client_certificate_data: String,
    pub client_key_data: String,
}

impl Access {
    /// The access in `kubeconfig`, the file k3s writes, reaching the API server at `server`.
    pub fn from_k3s(kubeconfig: &str, server: &str) -> Result<Self, KubeconfigError> {
        let invalid = |reason: String| KubeconfigError::Invalid {
            path: PathBuf::from("k3s.yaml"),
            reason,
        };
        let document = YamlLoader::load_from_str(kubeconfig)
            .map_err(|error| invalid(error.to_string()))?
            .into_iter()
            .next()
            .ok_or_else(|| invalid("it is empty".to_owned()))?;
        let field = |list: &str, entry: &str, key: &'static str| {
            document[list][0][entry][key]
                .as_str()
                .map(str::to_owned)
                .ok_or(KubeconfigError::Incomplete(key))
        };

        Ok(Self {
            server: server.to_owned(),
            certificate_authority_data: field("clusters", "cluster", "certificate-authority-data")?,
            client_certificate_data: field("users", "user", "client-certificate-data")?,
            client_key_data: field("users", "user", "client-key-data")?,
        })
    }
}

/// The kubeconfig kubectl writes to: the first file of `KUBECONFIG`, else `.kube/config` in the home directory.
pub fn default_path() -> Option<PathBuf> {
    let configured = std::env::var_os("KUBECONFIG").and_then(|list| std::env::split_paths(&list).next());

    configured
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| std::env::home_dir().map(|home| home.join(".kube").join("config")))
}

/// Adds the cluster, user and context `name` to the kubeconfig at `path`, or replaces them, leaving every other entry
/// and the current context as they were.
pub fn merge(path: &Path, name: &str, access: &Access) -> Result<(), KubeconfigError> {
    let mut root = load(path)?;
    root.entry(key("apiVersion")).or_insert_with(|| text("v1"));
    root.entry(key("kind")).or_insert_with(|| text("Config"));
    upsert(
        &mut root,
        "clusters",
        name,
        "cluster",
        hash(&[
            ("certificate-authority-data", &access.certificate_authority_data),
            ("server", &access.server),
        ]),
    );
    upsert(
        &mut root,
        "users",
        name,
        "user",
        hash(&[
            ("client-certificate-data", &access.client_certificate_data),
            ("client-key-data", &access.client_key_data),
        ]),
    );
    upsert(
        &mut root,
        "contexts",
        name,
        "context",
        hash(&[("cluster", name), ("user", name)]),
    );

    save(path, root)
}

/// kubectl's current context in the kubeconfig at `path`; `None` when it names none or the file does not exist.
pub fn current_context(path: &Path) -> Result<Option<String>, KubeconfigError> {
    Ok(load(path)?
        .get(&key("current-context"))
        .and_then(Yaml::as_str)
        .filter(|context| !context.is_empty())
        .map(str::to_owned))
}

/// Makes `context` kubectl's current context in the kubeconfig at `path`, or names none; returns the one it replaced.
pub fn set_current_context(path: &Path, context: Option<&str>) -> Result<Option<String>, KubeconfigError> {
    let previous = current_context(path)?;
    let mut root = load(path)?;
    match context {
        Some(context) => {
            root.insert(key("current-context"), text(context));
        }
        None => {
            root.remove(&key("current-context"));
        }
    }
    save(path, root)?;

    Ok(previous)
}

/// The top-level mapping of the kubeconfig at `path`; empty when the file does not exist.
fn load(path: &Path) -> Result<Hash, KubeconfigError> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Hash::new()),
        Err(source) => {
            return Err(KubeconfigError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let document = YamlLoader::load_from_str(&content)
        .map_err(|error| KubeconfigError::Invalid {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?
        .into_iter()
        .next()
        .unwrap_or(Yaml::Null);

    match document {
        Yaml::Hash(root) => Ok(root),
        Yaml::Null => Ok(Hash::new()),
        _ => Err(KubeconfigError::Invalid {
            path: path.to_path_buf(),
            reason: "its top level is not a mapping".to_owned(),
        }),
    }
}

/// Writes `document` through a staging file after copying the current file to the backup, so the user can return to
/// the file as it was before Contr's last change.
fn save(path: &Path, root: Hash) -> Result<(), KubeconfigError> {
    let write = |source| KubeconfigError::Write {
        path: path.to_path_buf(),
        source,
    };
    let mut content = String::new();
    YamlEmitter::new(&mut content)
        .dump(&Yaml::Hash(root))
        .map_err(|error| write(io::Error::other(error.to_string())))?;
    content.push('\n');

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(write)?;
    }
    if path.exists() {
        fs::copy(path, sibling(path, BACKUP_SUFFIX)).map_err(write)?;
    }
    let staging = sibling(path, "contr-new");
    contr_core::paths::write_private(&staging, content.as_bytes()).map_err(write)?;

    fs::rename(&staging, path).map_err(write)
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

/// Sets the entry named `name` of the list `list` to `{name, field: value}`, appending it when absent.
fn upsert(root: &mut Hash, list: &str, name: &str, field: &str, value: Yaml) {
    let entry = hash_of(vec![(key("name"), text(name)), (key(field), value)]);
    let entries = root.entry(key(list)).or_insert_with(|| Yaml::Array(Vec::new()));
    if !matches!(entries, Yaml::Array(_)) {
        *entries = Yaml::Array(Vec::new());
    }
    if let Yaml::Array(entries) = entries {
        match entries
            .iter_mut()
            .find(|existing| existing["name"].as_str() == Some(name))
        {
            Some(existing) => *existing = entry,
            None => entries.push(entry),
        }
    }
}

fn hash(pairs: &[(&str, &str)]) -> Yaml {
    hash_of(pairs.iter().map(|(name, value)| (key(name), text(value))).collect())
}

fn hash_of(pairs: Vec<(Yaml, Yaml)>) -> Yaml {
    Yaml::Hash(pairs.into_iter().collect())
}

fn key(name: &str) -> Yaml {
    text(name)
}

fn text(value: &str) -> Yaml {
    Yaml::String(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const K3S: &str = "apiVersion: v1
clusters:
- cluster:
    certificate-authority-data: Q0E=
    server: https://127.0.0.1:6443
  name: default
contexts:
- context:
    cluster: default
    user: default
  name: default
current-context: default
kind: Config
users:
- name: default
  user:
    client-certificate-data: Q0VSVA==
    client-key-data: S0VZ
";

    const USERS_OWN: &str = "apiVersion: v1
kind: Config
current-context: staging-eu
clusters:
- name: staging-eu
  cluster:
    server: https://staging.example.com
contexts:
- name: staging-eu
  context:
    cluster: staging-eu
    user: me
users:
- name: me
  user:
    token: secret
";

    fn access() -> Access {
        Access::from_k3s(K3S, "https://127.0.0.1:6443").unwrap()
    }

    #[test]
    fn reads_the_access_k3s_writes() {
        assert_eq!(
            access(),
            Access {
                server: "https://127.0.0.1:6443".to_owned(),
                certificate_authority_data: "Q0E=".to_owned(),
                client_certificate_data: "Q0VSVA==".to_owned(),
                client_key_data: "S0VZ".to_owned(),
            }
        );
        assert!(matches!(
            Access::from_k3s("clusters: []", ""),
            Err(KubeconfigError::Incomplete(_))
        ));
    }

    #[test]
    fn merges_beside_the_users_entries_and_keeps_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        fs::write(&path, USERS_OWN).unwrap();

        merge(&path, "contr", &access()).unwrap();
        merge(&path, "contr", &access()).unwrap();

        let merged = &YamlLoader::load_from_str(&fs::read_to_string(&path).unwrap()).unwrap()[0];
        let names = |list: &str| -> Vec<&str> {
            merged[list]
                .as_vec()
                .unwrap()
                .iter()
                .filter_map(|entry| entry["name"].as_str())
                .collect()
        };
        assert_eq!(names("clusters"), ["staging-eu", "contr"]);
        assert_eq!(names("users"), ["me", "contr"]);
        assert_eq!(names("contexts"), ["staging-eu", "contr"]);
        assert_eq!(merged["current-context"].as_str(), Some("staging-eu"));
        assert_eq!(merged["users"][0]["user"]["token"].as_str(), Some("secret"));
        assert_eq!(
            merged["clusters"][1]["cluster"]["server"].as_str(),
            Some("https://127.0.0.1:6443")
        );
        assert!(dir.path().join("config.contr-backup").exists());
    }

    #[test]
    fn creates_the_file_and_switches_contexts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".kube").join("config");

        merge(&path, "contr", &access()).unwrap();
        assert_eq!(current_context(&path).unwrap(), None);
        assert_eq!(set_current_context(&path, Some("contr")).unwrap(), None);
        assert_eq!(current_context(&path).unwrap().as_deref(), Some("contr"));
        assert_eq!(set_current_context(&path, None).unwrap().as_deref(), Some("contr"));
        assert_eq!(current_context(&path).unwrap(), None);
        assert_eq!(current_context(&dir.path().join("missing")).unwrap(), None);
    }

    #[test]
    fn refuses_files_that_are_not_mappings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        fs::write(&path, "- just\n- a list\n").unwrap();

        assert!(matches!(
            merge(&path, "contr", &access()),
            Err(KubeconfigError::Invalid { .. })
        ));
    }
}
