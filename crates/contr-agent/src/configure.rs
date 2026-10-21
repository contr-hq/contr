use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use contr_core::guest::{ConfigRejection, EngineConfig, UnknownKey};
use serde_json::{Map, Value};

use crate::engine::DOCKERD;

/// Directory of what the engine uses besides the guest's defaults.
const CONFIG_DIR: &str = "/etc/contr/engine";
/// Certificates the engine trusts besides the guest's own; `dockerd` finds them through `SSL_CERT_DIR`.
const CERTS_DIR: &str = "certs";
const HOST_CERTIFICATES: &str = "host.pem";
/// How `dockerd` reports keys it does not know; a key that is also a flag name reports its nested keys instead.
const UNKNOWN_DIRECTIVES: &str = "don't match any configuration option";
/// Close names tried as the replacement of one unknown key.
const SUGGESTION_TRIES: usize = 5;

/// The engine's `daemon.json`, once `configure` wrote one.
pub(crate) fn daemon_file() -> Option<PathBuf> {
    Some(Path::new(CONFIG_DIR).join("daemon.json")).filter(|path| path.is_file())
}

/// Directory whose certificates the engine trusts besides the guest's own.
pub(crate) fn certificates_dir() -> PathBuf {
    Path::new(CONFIG_DIR).join(CERTS_DIR)
}

/// `dockerd` refused a configuration.
#[derive(Debug)]
pub(crate) struct Rejected(ConfigRejection);

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the engine rejects its configuration: {}", self.0.reason)
    }
}

impl std::error::Error for Rejected {}

/// Judges configuration files the way the engine will.
trait Judge {
    /// `None` when the engine accepts the configuration file at `path`, else its reason.
    fn verdict(&self, path: &Path) -> anyhow::Result<Option<String>>;
    /// Keys `daemon.json` may hold.
    fn key_names(&self) -> anyhow::Result<Vec<String>>;
}

struct Dockerd;

impl Judge for Dockerd {
    fn verdict(&self, path: &Path) -> anyhow::Result<Option<String>> {
        let output = Command::new(DOCKERD)
            .arg("--validate")
            .arg("--config-file")
            .arg(path)
            .output()
            .context("cannot run dockerd")?;

        Ok((!output.status.success()).then(|| String::from_utf8_lossy(&output.stderr).trim().to_owned()))
    }

    fn key_names(&self) -> anyhow::Result<Vec<String>> {
        let output = Command::new(DOCKERD)
            .arg("--help")
            .output()
            .context("cannot run dockerd")?;

        Ok(flag_keys(&String::from_utf8_lossy(&output.stdout)))
    }
}

/// Writes the [`EngineConfig`] on standard input for the engine's next start once `dockerd` accepts it; with `check`,
/// only asks `dockerd`. A rejection also goes to standard output as a [`ConfigRejection`].
pub(crate) fn configure(check: bool) -> anyhow::Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let config: EngineConfig = serde_json::from_str(&input).context("standard input holds no engine configuration")?;
    let dir = Path::new(CONFIG_DIR);
    let configured = if check {
        self::check(dir, &config, &Dockerd)
    } else {
        write(dir, &config, &Dockerd)
    };
    if let Some(Rejected(rejection)) = configured.as_ref().err().and_then(|error| error.downcast_ref()) {
        println!("{}", serde_json::to_string(rejection)?);
    }

    configured
}

fn check(dir: &Path, config: &EngineConfig, judge: &impl Judge) -> anyhow::Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    match examine(dir, &config.daemon, judge)? {
        Some(rejection) => Err(Rejected(rejection).into()),
        None => Ok(()),
    }
}

fn write(dir: &Path, config: &EngineConfig, judge: &impl Judge) -> anyhow::Result<()> {
    let certificates = dir.join(CERTS_DIR);
    fs::create_dir_all(&certificates).with_context(|| format!("cannot create {}", certificates.display()))?;
    if let Some(rejection) = examine(dir, &config.daemon, judge)? {
        return Err(Rejected(rejection).into());
    }

    let daemon = dir.join("daemon.json");
    let staged = daemon.with_extension("json.new");
    fs::write(&staged, serde_json::to_vec_pretty(&config.daemon)?)
        .with_context(|| format!("cannot write {}", staged.display()))?;
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

/// Why `judge` rejects `daemon`, staged in `dir`, naming each top-level key it does not know with a key it accepts
/// in its place; `None` when it accepts `daemon`.
fn examine(dir: &Path, daemon: &Map<String, Value>, judge: &impl Judge) -> anyhow::Result<Option<ConfigRejection>> {
    let staged = dir.join("daemon.json.check");
    let verdict = |daemon: &Map<String, Value>| {
        fs::write(&staged, serde_json::to_vec_pretty(daemon)?)
            .with_context(|| format!("cannot write {}", staged.display()))?;
        let verdict = judge.verdict(&staged);
        let _ = fs::remove_file(&staged);
        verdict
    };
    let Some(reason) = verdict(daemon)? else {
        return Ok(None);
    };

    let mut unknown_keys = Vec::new();
    if reason.contains(UNKNOWN_DIRECTIVES) {
        let names = judge.key_names()?;
        for (key, value) in daemon {
            let alone = |key: &str| Map::from_iter([(key.to_owned(), value.clone())]);
            if !is_unknown(verdict(&alone(key))?.as_deref()) {
                continue;
            }
            let mut suggestion = None;
            for candidate in close_names(key, &names) {
                if !is_unknown(verdict(&alone(&candidate))?.as_deref()) {
                    suggestion = Some(candidate);
                    break;
                }
            }
            unknown_keys.push(UnknownKey {
                key: key.clone(),
                suggestion,
            });
        }
    }
    let file_prefix = format!("{}: ", staged.display());
    let reason = reason
        .split_once(&file_prefix)
        .map_or(reason.as_str(), |(_, rest)| rest);

    Ok(Some(ConfigRejection {
        reason: reason.to_owned(),
        unknown_keys,
    }))
}

fn is_unknown(verdict: Option<&str>) -> bool {
    verdict.is_some_and(|reason| reason.contains(UNKNOWN_DIRECTIVES))
}

/// `daemon.json` keys of the long flags in `dockerd --help`: each flag's name, and its plural, which repeatable flags
/// use there, as `log-opts` for `--log-opt`.
fn flag_keys(help: &str) -> Vec<String> {
    let mut keys = BTreeSet::new();
    for name in help
        .split_whitespace()
        .filter_map(|token| token.strip_prefix("--"))
        .map(|name| name.trim_end_matches(','))
        .filter(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
    {
        let plural = match name.strip_suffix('y') {
            Some(stem) => format!("{stem}ies"),
            None => format!("{name}s"),
        };
        keys.insert(plural);
        keys.insert(name.to_owned());
    }

    keys.into_iter().collect()
}

/// Names within a small edit distance of `key`, closest first.
fn close_names(key: &str, names: &[String]) -> Vec<String> {
    let limit = (key.chars().count() / 3).clamp(1, 3);
    let mut close: Vec<(usize, &String)> = names
        .iter()
        .filter(|name| *name != key)
        .map(|name| (distance(key, name), name))
        .filter(|(distance, _)| *distance <= limit)
        .collect();
    close.sort();

    close
        .into_iter()
        .take(SUGGESTION_TRIES)
        .map(|(_, name)| name.clone())
        .collect()
}

/// Edits that turn `a` into `b`: insertions, deletions, substitutions and swaps of adjacent characters.
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut rows = vec![(0..=b.len()).collect::<Vec<_>>(); a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (rows[i - 1][j] + 1)
                .min(rows[i][j - 1] + 1)
                .min(rows[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = best;
        }
    }

    rows[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;

    /// Judges like `dockerd` with a few keys: `log-opt` is only a flag, so its nested keys are the unknown ones.
    #[derive(Default)]
    struct FakeDockerd {
        judged: RefCell<usize>,
    }

    impl Judge for FakeDockerd {
        fn verdict(&self, path: &Path) -> anyhow::Result<Option<String>> {
            *self.judged.borrow_mut() += 1;
            let daemon: Map<String, Value> = serde_json::from_slice(&fs::read(path)?)?;
            let mut unknown = Vec::new();
            for (key, value) in &daemon {
                match (key.as_str(), value) {
                    ("debug" | "log-opts" | "labels", _) => {}
                    ("log-opt", Value::Object(nested)) => unknown.extend(nested.keys().cloned()),
                    _ => unknown.push(key.clone()),
                }
            }

            Ok((!unknown.is_empty()).then(|| {
                format!(
                    "unable to configure the Docker daemon with file {}: the following directives don't match any \
                     configuration option: {}",
                    path.display(),
                    unknown.join(", ")
                )
            }))
        }

        fn key_names(&self) -> anyhow::Result<Vec<String>> {
            Ok(flag_keys(
                "  -D, --debug          Enable debug mode\n      --label list    Set labels\n      --log-opt map   \
                 Default log driver options\n      --insecure-registry list   Insecure registries\n",
            ))
        }
    }

    fn config(daemon: &Value, certificates: &str) -> EngineConfig {
        EngineConfig {
            daemon: daemon.as_object().cloned().unwrap_or_default(),
            certificates: certificates.to_owned(),
        }
    }

    #[test]
    fn writes_the_configuration_and_removes_certificates_not_given() {
        let dir = tempfile::tempdir().unwrap();
        let dockerd = FakeDockerd::default();

        write(dir.path(), &config(&json!({"labels": ["a=b"]}), "PEM"), &dockerd).unwrap();
        let daemon: Value = serde_json::from_slice(&fs::read(dir.path().join("daemon.json")).unwrap()).unwrap();
        assert_eq!(daemon, json!({"labels": ["a=b"]}));
        assert_eq!(fs::read_to_string(dir.path().join("certs/host.pem")).unwrap(), "PEM");

        write(dir.path(), &config(&json!({}), ""), &dockerd).unwrap();
        assert!(!dir.path().join("certs/host.pem").exists());
        assert!(dir.path().join("certs").is_dir());
    }

    #[test]
    fn names_unknown_keys_with_the_key_the_engine_takes_instead() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = config(
            &json!({"debgu": true, "debug": false, "log-opt": {"max-size": "10m"}, "zzz": 1}),
            "",
        );

        let error = check(dir.path(), &daemon, &FakeDockerd::default()).unwrap_err();

        let Some(Rejected(rejection)) = error.downcast_ref() else {
            panic!("{error:#}");
        };
        assert_eq!(
            rejection.reason,
            "the following directives don't match any configuration option: debgu, max-size, zzz"
        );
        assert_eq!(
            rejection.unknown_keys,
            [
                UnknownKey {
                    key: "debgu".to_owned(),
                    suggestion: Some("debug".to_owned()),
                },
                UnknownKey {
                    key: "log-opt".to_owned(),
                    suggestion: Some("log-opts".to_owned()),
                },
                UnknownKey {
                    key: "zzz".to_owned(),
                    suggestion: None,
                },
            ]
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0, "a check writes nothing");
    }

    #[test]
    fn an_accepted_configuration_is_judged_once() {
        let dir = tempfile::tempdir().unwrap();
        let dockerd = FakeDockerd::default();

        check(dir.path(), &config(&json!({"debug": true}), ""), &dockerd).unwrap();

        assert_eq!(*dockerd.judged.borrow(), 1);
    }

    #[test]
    fn keeps_the_previous_configuration_when_the_engine_rejects_the_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let dockerd = FakeDockerd::default();
        write(dir.path(), &config(&json!({"debug": true}), ""), &dockerd).unwrap();

        let error = write(dir.path(), &config(&json!({"nope": 1}), ""), &dockerd).unwrap_err();

        assert!(error.is::<Rejected>());
        let daemon: Value = serde_json::from_slice(&fs::read(dir.path().join("daemon.json")).unwrap()).unwrap();
        assert_eq!(daemon, json!({"debug": true}));
    }

    #[test]
    fn flag_keys_include_the_plural_daemon_json_uses() {
        let keys = flag_keys("  -D, --debug   Enable\n      --insecure-registry list  x\n      --log-opt map  y");

        for key in ["debug", "insecure-registries", "log-opt", "log-opts"] {
            assert!(keys.contains(&key.to_owned()), "{key} in {keys:?}");
        }
    }

    #[test]
    fn counts_swapped_letters_as_one_edit() {
        assert_eq!(distance("debgu", "debug"), 1);
        assert_eq!(distance("log-opt", "log-opts"), 1);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(close_names("debgu", &["debug".to_owned(), "dns".to_owned()]), ["debug"]);
    }
}
