//! Typed daemon settings: defaults, overridden by the values the user changed, which the daemon persists per user as
//! one JSON object, overridden in turn by values an administrator fixes in a policy object of the same shape.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::PathBuf;

use contr_core::proxy::{exclusions, server_url};
use http::Uri;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Configurable settings with a default for every field. Paths name fields in `snake_case`, such as
/// `engine.start_with_daemon`; stored JSON names them in camelCase.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Configured {
    pub engine: Engine,
    pub proxy: Proxy,
    pub certificates: Certificates,
    pub registry: Registry,
    pub kubernetes: Kubernetes,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Engine {
    pub start_with_daemon: bool,
    pub serve_default_pipe: bool,
    /// The engine's `daemon.json`, without the keys of [`CONTR_DAEMON_KEYS`].
    pub daemon_json: Map<String, Value>,
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            start_with_daemon: false,
            serve_default_pipe: true,
            daemon_json: Map::new(),
        }
    }
}

/// `daemon.json` keys Contr sets itself, with what a user changes instead.
pub const CONTR_DAEMON_KEYS: [(&str, &str); 6] = [
    ("hosts", "Contr chooses where the engine listens"),
    ("data-root", "Contr chooses where the engine keeps its data"),
    ("host-gateway-ip", "Contr points host.docker.internal at the host"),
    ("host-gateway-ips", "Contr points host.docker.internal at the host"),
    (
        "proxies",
        "set proxy.mode, proxy.http, proxy.https and proxy.exclude instead",
    ),
    ("registry-mirrors", "set registry.mirrors instead"),
];

/// How the engine reaches servers outside the machine.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Proxy {
    pub mode: ProxyMode,
    /// Manual mode: proxy for `http://` destinations, as `http://proxy.example.com:3128`.
    pub http: String,
    /// Manual mode: proxy for `https://` destinations.
    pub https: String,
    /// Manual mode: destinations reached directly, as a `NO_PROXY` list.
    pub exclude: String,
    /// New containers and builds also receive the engine's proxy as `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY`.
    pub pass_to_containers: bool,
}

impl Proxy {
    /// The servers of manual mode; a server without a scheme speaks HTTP.
    pub fn manual(&self) -> contr_core::proxy::Proxy {
        let server = |text: &str| Some(text.trim()).filter(|text| !text.is_empty()).map(server_url);

        contr_core::proxy::Proxy {
            http: server(&self.http),
            https: server(&self.https),
            exclude: exclusions(&self.exclude),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProxyMode {
    /// The operating system's proxy.
    #[default]
    System,
    /// The servers of [`Proxy`].
    Manual,
    /// Direct connections.
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Certificates {
    /// The engine also trusts the root certificates the operating system trusts.
    pub trust_system: bool,
}

impl Default for Certificates {
    fn default() -> Self {
        Self { trust_system: true }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Registry {
    /// URLs of Docker Hub mirrors the engine tries before Docker Hub.
    pub mirrors: Vec<String>,
}

/// The single-node cluster.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Kubernetes {
    pub enabled: bool,
    /// One of [`contr_core::kube::VERSIONS`]; empty runs the default.
    pub version: String,
    pub traefik: bool,
}

impl Kubernetes {
    /// The version the cluster runs.
    pub fn version(&self) -> &str {
        if self.version.is_empty() {
            contr_core::kube::VERSIONS[0]
        } else {
            &self.version
        }
    }
}

/// Every path a user may change and an administrator may fix.
pub const PATHS: [&str; 13] = [
    "engine.start_with_daemon",
    "engine.serve_default_pipe",
    "engine.daemon_json",
    "proxy.mode",
    "proxy.http",
    "proxy.https",
    "proxy.exclude",
    "proxy.pass_to_containers",
    "certificates.trust_system",
    "registry.mirrors",
    "kubernetes.enabled",
    "kubernetes.version",
    "kubernetes.traefik",
];

/// URL schemes of a proxy server the engine connects through.
const PROXY_SCHEMES: [&str; 4] = ["http", "https", "socks5", "socks5h"];

impl Configured {
    fn validate(&self) -> Result<(), SettingsError> {
        for (path, server) in [("proxy.http", &self.proxy.http), ("proxy.https", &self.proxy.https)] {
            if !server.trim().is_empty() {
                check_url(path, &server_url(server), &PROXY_SCHEMES)?;
            }
        }
        for mirror in &self.registry.mirrors {
            check_url("registry.mirrors", mirror, &["http", "https"])?;
        }
        let version = &self.kubernetes.version;
        if !version.is_empty() && !contr_core::kube::VERSIONS.contains(&version.as_str()) {
            return Err(SettingsError::InvalidValue {
                path: "kubernetes.version",
                reason: format!("{version} is not one of {}", contr_core::kube::VERSIONS.join(", ")),
            });
        }
        if let Some((key, instead)) = CONTR_DAEMON_KEYS
            .iter()
            .find(|(key, _)| self.engine.daemon_json.contains_key(*key))
        {
            return Err(SettingsError::InvalidValue {
                path: "engine.daemon_json",
                reason: format!("{key} is not available: {instead}"),
            });
        }

        Ok(())
    }
}

fn check_url(path: &'static str, url: &str, schemes: &[&str]) -> Result<(), SettingsError> {
    let invalid = |reason: String| SettingsError::InvalidValue { path, reason };
    let uri: Uri = url.parse().map_err(|_| invalid(format!("{url:?} is not a URL")))?;
    if !uri.scheme_str().is_some_and(|scheme| schemes.contains(&scheme)) {
        return Err(invalid(format!("{url:?} does not use {}", schemes.join(", "))));
    }
    if uri.host().is_none_or(str::is_empty) {
        return Err(invalid(format!("{url:?} names no server")));
    }

    Ok(())
}

/// Effective settings and the paths an administrator fixes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Effective {
    pub configured: Configured,
    pub locked: BTreeSet<&'static str>,
}

/// Combines the defaults, the values `user` stored and the administrator's `policy`, which wins.
pub fn effective(user: &Settings, policy: &Map<String, Value>) -> Result<Effective, SettingsError> {
    let mut merged = serde_json::to_value(Configured::default()).map_err(SettingsError::Value)?;
    let mut locked = BTreeSet::new();
    for path in PATHS {
        let keys = keys(path);
        if let Some(value) = lookup(&user.stored, &keys) {
            put(&mut merged, &keys, value.clone());
        }
        if let Some(value) = lookup(policy, &keys) {
            put(&mut merged, &keys, value.clone());
            locked.insert(path);
        }
    }

    let configured: Configured = serde_json::from_value(merged).map_err(SettingsError::Value)?;
    configured.validate()?;

    Ok(Effective { configured, locked })
}

/// The administrator's policy: a JSON object shaped like the stored settings.
pub fn parse_policy(text: &str) -> Result<Map<String, Value>, SettingsError> {
    match serde_json::from_str(text).map_err(SettingsError::Value)? {
        Value::Object(policy) => Ok(policy),
        _ => Err(SettingsError::Value(serde::de::Error::custom(
            "the policy is not a JSON object",
        ))),
    }
}

/// What the user stored. Keys this release does not know survive a rewrite, so a downgrade loses nothing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// WSL distributions of the user that reach the engine.
    #[serde(default)]
    pub wsl_integrations: BTreeSet<String>,
    /// Compose projects the user added, each by its Compose files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compose_projects: Vec<ComposeProject>,
    /// kubectl's current context before the user made the cluster's context current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_kube_context: Option<String>,
    #[serde(flatten)]
    stored: Map<String, Value>,
}

impl Settings {
    /// Stores `value` for `path`, one of [`PATHS`]; returns whether the stored value changed.
    pub fn set(&mut self, path: &str, value: Value) -> Result<bool, SettingsError> {
        let path = known(path)?;
        let keys = keys(path);
        let changed = lookup(&self.stored, &keys) != Some(&value);
        let mut root = Value::Object(std::mem::take(&mut self.stored));
        put(&mut root, &keys, value);
        if let Value::Object(stored) = root {
            self.stored = stored;
        }

        Ok(changed)
    }

    /// Forgets every value of [`PATHS`] the user stored; returns the paths that had one.
    pub fn clear(&mut self) -> Vec<&'static str> {
        let stored: Vec<&'static str> = PATHS
            .into_iter()
            .filter(|path| lookup(&self.stored, &keys(path)).is_some())
            .collect();
        for path in &stored {
            remove(&mut self.stored, &keys(path));
        }

        stored
    }
}

/// A Compose project the user added.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposeProject {
    /// Compose files in the order Compose merges them.
    pub config_files: Vec<String>,
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
    #[error("{0} is not a setting")]
    UnknownPath(String),
    #[error("a setting has a value of the wrong type")]
    Value(#[source] serde_json::Error),
    #[error("{path}: {reason}")]
    InvalidValue { path: &'static str, reason: String },
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

fn known(path: &str) -> Result<&'static str, SettingsError> {
    PATHS
        .into_iter()
        .find(|known| *known == path)
        .ok_or_else(|| SettingsError::UnknownPath(path.to_owned()))
}

/// JSON keys of `path`: each `snake_case` segment in camelCase.
fn keys(path: &str) -> Vec<String> {
    path.split('.')
        .map(|segment| {
            let mut parts = segment.split('_');
            let first = parts.next().unwrap_or_default().to_owned();
            parts.fold(first, |mut key, part| {
                let mut chars = part.chars();
                if let Some(initial) = chars.next() {
                    key.extend(initial.to_uppercase());
                    key.push_str(chars.as_str());
                }
                key
            })
        })
        .collect()
}

fn lookup<'a>(object: &'a Map<String, Value>, keys: &[String]) -> Option<&'a Value> {
    let (leaf, parents) = keys.split_last()?;
    let mut object = object;
    for key in parents {
        object = object.get(key)?.as_object()?;
    }
    object.get(leaf)
}

fn put(root: &mut Value, keys: &[String], value: Value) {
    let Some((leaf, parents)) = keys.split_last() else {
        return;
    };
    let mut node = root;
    for key in parents {
        if !node.is_object() {
            *node = Value::Object(Map::new());
        }
        let Value::Object(object) = node else {
            return;
        };
        node = object.entry(key.clone()).or_insert_with(|| Value::Object(Map::new()));
    }
    if !node.is_object() {
        *node = Value::Object(Map::new());
    }
    if let Value::Object(object) = node {
        object.insert(leaf.clone(), value);
    }
}

fn remove(object: &mut Map<String, Value>, keys: &[String]) {
    let Some((leaf, parents)) = keys.split_last() else {
        return;
    };
    let mut object = object;
    for key in parents {
        let Some(Value::Object(child)) = object.get_mut(key) else {
            return;
        };
        object = child;
    }
    object.remove(leaf);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn policy(value: &Value) -> Map<String, Value> {
        parse_policy(&value.to_string()).unwrap()
    }

    #[test]
    fn a_missing_file_holds_the_defaults() {
        let dir = tempfile::tempdir().unwrap();

        let settings = SettingsFile::new(dir.path().join("settings.json")).load().unwrap();

        assert_eq!(settings, Settings::default());
        assert_eq!(effective(&settings, &Map::new()).unwrap(), Effective::default());
    }

    #[test]
    fn user_values_override_defaults_and_policy_overrides_both() {
        let mut settings = Settings::default();
        assert!(settings.set("engine.start_with_daemon", json!(true)).unwrap());
        assert!(settings.set("engine.serve_default_pipe", json!(false)).unwrap());
        assert!(!settings.set("engine.serve_default_pipe", json!(false)).unwrap());

        let mine = effective(&settings, &Map::new()).unwrap();
        assert!(mine.configured.engine.start_with_daemon);
        assert!(!mine.configured.engine.serve_default_pipe);
        assert!(mine.locked.is_empty());

        let managed = effective(&settings, &policy(&json!({"engine": {"startWithDaemon": false}}))).unwrap();
        assert!(!managed.configured.engine.start_with_daemon);
        assert_eq!(managed.locked, BTreeSet::from(["engine.start_with_daemon"]));
    }

    #[test]
    fn clearing_forgets_only_configurable_values() {
        let mut settings: Settings =
            serde_json::from_value(json!({"wslIntegrations": ["Ubuntu"], "futureKey": 1})).unwrap();
        settings.set("engine.start_with_daemon", json!(true)).unwrap();

        assert_eq!(settings.clear(), ["engine.start_with_daemon"]);
        assert!(settings.clear().is_empty());

        let written = serde_json::to_value(&settings).unwrap();
        assert_eq!(
            written,
            json!({"wslIntegrations": ["Ubuntu"], "futureKey": 1, "engine": {}})
        );
    }

    #[test]
    fn refuses_unknown_paths_and_values_of_the_wrong_type() {
        let mut settings = Settings::default();

        assert!(matches!(
            settings.set("engine.nope", json!(true)),
            Err(SettingsError::UnknownPath(_))
        ));
        settings.set("engine.start_with_daemon", json!("yes")).unwrap();
        assert!(matches!(
            effective(&settings, &Map::new()),
            Err(SettingsError::Value(_))
        ));
        assert!(parse_policy("[]").is_err());
    }

    #[test]
    fn the_engine_uses_the_system_proxy_and_certificates_by_default() {
        let configured = effective(&Settings::default(), &Map::new()).unwrap().configured;

        assert_eq!(configured.proxy.mode, ProxyMode::System);
        assert!(!configured.proxy.pass_to_containers);
        assert!(configured.certificates.trust_system);
        assert!(configured.registry.mirrors.is_empty());
    }

    #[test]
    fn manual_proxies_take_servers_without_a_scheme() {
        let mut settings = Settings::default();
        settings.set("proxy.mode", json!("manual")).unwrap();
        settings.set("proxy.http", json!("proxy.corp:3128")).unwrap();
        settings.set("proxy.exclude", json!(".corp, 10.0.0.0/8")).unwrap();

        let proxy = effective(&settings, &Map::new()).unwrap().configured.proxy;

        assert_eq!(proxy.mode, ProxyMode::Manual);
        assert_eq!(
            proxy.manual(),
            contr_core::proxy::Proxy {
                http: Some("http://proxy.corp:3128".to_owned()),
                https: None,
                exclude: vec![".corp".to_owned(), "10.0.0.0/8".to_owned()],
            }
        );
    }

    #[test]
    fn passing_the_proxy_to_containers_is_stored_and_may_be_locked() {
        let mut settings = Settings::default();
        assert!(settings.set("proxy.pass_to_containers", json!(true)).unwrap());

        assert_eq!(
            serde_json::to_value(&settings).unwrap(),
            json!({"wslIntegrations": [], "proxy": {"passToContainers": true}})
        );
        assert!(
            effective(&settings, &Map::new())
                .unwrap()
                .configured
                .proxy
                .pass_to_containers
        );

        let managed = effective(&settings, &policy(&json!({"proxy": {"passToContainers": false}}))).unwrap();
        assert!(!managed.configured.proxy.pass_to_containers);
        assert_eq!(managed.locked, BTreeSet::from(["proxy.pass_to_containers"]));
    }

    #[test]
    fn refuses_unusable_proxies_and_mirrors() {
        let refused = |path: &str, value: Value| {
            let mut settings = Settings::default();
            settings.set(path, value).unwrap();
            effective(&settings, &Map::new()).unwrap_err().to_string()
        };

        assert_eq!(
            refused("proxy.https", json!("ftp://proxy:21")),
            r#"proxy.https: "ftp://proxy:21" does not use http, https, socks5, socks5h"#
        );
        assert!(refused("proxy.http", json!("http://proxy with space:1")).starts_with("proxy.http: "));
        assert_eq!(
            refused("registry.mirrors", json!(["https://mirror.corp", "mirror.corp"])),
            r#"registry.mirrors: "mirror.corp" does not use http, https"#
        );
        assert!(refused("proxy.mode", json!("pac")).contains("wrong type"));
    }

    #[test]
    fn daemon_json_takes_what_contr_does_not_set() {
        let with = |daemon: Value| {
            let mut settings = Settings::default();
            settings.set("engine.daemon_json", daemon).unwrap();
            effective(&settings, &Map::new())
        };

        let configured = with(json!({"debug": true, "log-level": "warn"})).unwrap().configured;
        assert_eq!(configured.engine.daemon_json["log-level"], "warn");
        assert_eq!(
            with(json!({"debug": true, "registry-mirrors": []}))
                .unwrap_err()
                .to_string(),
            "engine.daemon_json: registry-mirrors is not available: set registry.mirrors instead"
        );
        assert!(matches!(with(json!("{}")), Err(SettingsError::Value(_))));
    }

    #[test]
    fn kubernetes_runs_a_listed_version() {
        let mut settings = Settings::default();
        assert_eq!(
            effective(&settings, &Map::new())
                .unwrap()
                .configured
                .kubernetes
                .version(),
            contr_core::kube::VERSIONS[0]
        );

        settings.set("kubernetes.version", json!("v1.36.5")).unwrap();
        assert_eq!(
            effective(&settings, &Map::new())
                .unwrap()
                .configured
                .kubernetes
                .version(),
            "v1.36.5"
        );
        settings.set("kubernetes.version", json!("v1.20.0")).unwrap();
        assert!(
            effective(&settings, &Map::new())
                .unwrap_err()
                .to_string()
                .starts_with("kubernetes.version: v1.20.0 is not one of ")
        );
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
        settings.compose_projects.push(ComposeProject {
            config_files: vec![r"C:\src\shop\compose.yaml".to_owned()],
        });
        settings.set("engine.start_with_daemon", json!(true)).unwrap();
        file.save(&settings).unwrap();

        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["wslIntegrations"], json!(["Debian", "Ubuntu"]));
        assert_eq!(written["futureKey"]["on"], true);
        assert_eq!(written["engine"]["startWithDaemon"], true);
        assert_eq!(
            written["composeProjects"],
            json!([{"configFiles": [r"C:\src\shop\compose.yaml"]}])
        );
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
