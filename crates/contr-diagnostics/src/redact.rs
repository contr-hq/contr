use std::borrow::Cow;
use std::path::Path;

use regex::{Regex, RegexBuilder};
use serde_json::Value;

const REDACTED: &str = "<redacted>";

/// JSON keys whose values are secrets, matched case-insensitively as substrings.
const SECRET_KEYS: [&str; 9] = [
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "privatekey",
    "private_key",
    "authorization",
];
/// JSON keys whose values are secrets, matched case-insensitively as whole keys; Docker's config.json names
/// registry credentials `auth`.
const SECRET_EXACT_KEYS: [&str; 2] = ["auth", "credentials"];
/// User names too common to replace without mangling ordinary words.
const KEPT_USER_NAMES: [&str; 3] = ["root", "admin", "user"];
const MIN_REPLACED_NAME: usize = 3;

/// Who the bundle must not identify.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    pub home: Option<String>,
    pub user: Option<String>,
    pub host: Option<String>,
}

impl Identity {
    /// The user the daemon runs as and the computer it runs on, from the environment.
    pub fn current() -> Self {
        let var = |names: &[&str]| {
            names
                .iter()
                .find_map(|name| std::env::var(name).ok())
                .filter(|value| !value.is_empty())
        };
        Self {
            home: var(&["HOME", "USERPROFILE"]),
            user: var(&["USER", "USERNAME", "LOGNAME"]),
            host: var(&["COMPUTERNAME", "HOSTNAME"]).or_else(|| {
                std::fs::read_to_string(Path::new("/etc/hostname"))
                    .ok()
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty())
            }),
        }
    }
}

/// Replaces credentials, the user's home directory, name and computer name, and Windows account SIDs.
pub struct Redactor {
    rules: Vec<(Regex, String)>,
}

impl Redactor {
    pub fn new(identity: &Identity) -> Result<Self, regex::Error> {
        let mut rules = vec![
            // Credentials in a URL, as in http://user:secret@proxy:3128; a password may hold an unescaped @.
            (
                pattern(r#"\b([a-z][a-z0-9+.-]*://)[^\s/"']+@"#)?,
                format!("${{1}}{REDACTED}@"),
            ),
            (
                pattern(
                    r#"\b((?:proxy-)?authorization["']?\s*[:=]\s*["']?)(?:(?:basic|bearer|digest|token)\s+)?[^\s"',;}]+"#,
                )?,
                format!("${{1}}{REDACTED}"),
            ),
            (
                pattern(
                    r#"\b([a-z0-9_.-]*(?:password|passwd|secret|token|api[_-]?key|private[_-]?key)["']?\s*[:=]\s*["']?)[^\s"',;&}]+"#,
                )?,
                format!("${{1}}{REDACTED}"),
            ),
            (pattern(r"\bS-1-5-21(?:-\d+){4}\b")?, "<sid>".to_owned()),
        ];

        if let Some(home) = identity.home.as_deref().filter(|home| home.len() > 1) {
            let home = home.trim_end_matches(['/', '\\']);
            for spelling in [home.to_owned(), home.replace('\\', r"\\")] {
                rules.push((pattern(&format!(r"{}\b", regex::escape(&spelling)))?, "~".to_owned()));
            }
        }
        let user = identity
            .user
            .as_deref()
            .filter(|user| user.len() >= MIN_REPLACED_NAME && !KEPT_USER_NAMES.contains(&user.to_lowercase().as_str()));
        // The computer's name often starts with the user's, so it goes first.
        for (name, replacement) in [(identity.host.as_deref(), "<host>"), (user, "<user>")] {
            if let Some(name) = name.filter(|name| name.len() >= MIN_REPLACED_NAME) {
                rules.push((
                    pattern(&format!(r"\b{}\b", regex::escape(name)))?,
                    replacement.to_owned(),
                ));
            }
        }

        Ok(Self { rules })
    }

    pub fn text(&self, text: &str) -> String {
        self.rules
            .iter()
            .fold(Cow::Borrowed(text), |text, (pattern, replacement)| {
                match pattern.replace_all(&text, replacement.as_str()) {
                    Cow::Borrowed(_) => text,
                    Cow::Owned(replaced) => Cow::Owned(replaced),
                }
            })
            .into_owned()
    }

    /// Replaces the values of secret keys outright and redacts every other string.
    pub fn json(&self, value: Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(&text)),
            Value::Array(items) => Value::Array(items.into_iter().map(|item| self.json(item)).collect()),
            Value::Object(fields) => Value::Object(
                fields
                    .into_iter()
                    .map(|(key, value)| {
                        let value = if is_secret(&key) && !value.is_null() {
                            Value::String(REDACTED.to_owned())
                        } else {
                            self.json(value)
                        };
                        (self.text(&key), value)
                    })
                    .collect(),
            ),
            other => other,
        }
    }
}

fn pattern(source: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(source).case_insensitive(true).build()
}

fn is_secret(key: &str) -> bool {
    let key = key.to_lowercase();
    SECRET_KEYS.iter().any(|secret| key.contains(secret)) || SECRET_EXACT_KEYS.contains(&key.as_str())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn redactor() -> Redactor {
        Redactor::new(&Identity {
            home: Some("/home/jdoe".to_owned()),
            user: Some("jdoe".to_owned()),
            host: Some("jdoe-laptop".to_owned()),
        })
        .unwrap()
    }

    #[test]
    fn removes_credentials_from_urls_headers_and_assignments() {
        let redactor = redactor();

        for (text, expected) in [
            (
                "proxy http://corp%5Cjane:p@ss@proxy.corp:3128 failed",
                "proxy http://<redacted>@proxy.corp:3128 failed",
            ),
            (
                "HTTPS_PROXY=https://svc:hunter2@10.0.0.1:8080/",
                "HTTPS_PROXY=https://<redacted>@10.0.0.1:8080/",
            ),
            ("Authorization: Bearer eyJhbGciOi.x.y", "Authorization: <redacted>"),
            (
                "Proxy-Authorization: Basic dXNlcjpwYXNz",
                "Proxy-Authorization: <redacted>",
            ),
            (
                "login --password=s3cret --user x",
                "login --password=<redacted> --user x",
            ),
            (r#"{"registryToken": "abc123"}"#, r#"{"registryToken": "<redacted>"}"#),
            ("GITHUB_TOKEN=ghp_abc&next=1", "GITHUB_TOKEN=<redacted>&next=1"),
            ("api_key: AKIA1234", "api_key: <redacted>"),
        ] {
            assert_eq!(redactor.text(text), expected, "{text}");
        }
    }

    #[test]
    fn keeps_what_only_looks_like_a_secret() {
        let redactor = redactor();

        for text in [
            "https://registry-1.docker.io/v2/library/alpine/manifests/3.24",
            "docker.io/library/nginx@sha256:0123abcd",
            "the token bucket refilled",
            "rootless engine at unix:///run/user/1000/docker.sock",
        ] {
            assert_eq!(redactor.text(text), text);
        }
    }

    #[test]
    fn hides_the_user_their_home_and_computer() {
        let redactor = redactor();

        assert_eq!(
            redactor.text("jdoe on jdoe-laptop reads /home/jdoe/src/app and /home/jdoe2"),
            "<user> on <host> reads ~/src/app and /home/jdoe2"
        );
        assert_eq!(
            redactor.text(r"pipe \\.\pipe\contr-S-1-5-21-1004336348-1177238915-682003330-1001-api"),
            r"pipe \\.\pipe\contr-<sid>-api"
        );
    }

    #[test]
    fn hides_a_windows_home_in_paths_and_json_escapes() {
        let redactor = Redactor::new(&Identity {
            home: Some(r"C:\Users\Jane.Doe".to_owned()),
            user: Some("Jane.Doe".to_owned()),
            host: None,
        })
        .unwrap();

        assert_eq!(
            redactor.text(r#"bind C:\Users\Jane.Doe\src:/w in "C:\\Users\\Jane.Doe\\.docker""#),
            r#"bind ~\src:/w in "~\\.docker""#
        );
    }

    #[test]
    fn keeps_short_and_common_user_names() {
        let redactor = Redactor::new(&Identity {
            home: Some("/root".to_owned()),
            user: Some("root".to_owned()),
            host: Some("pc".to_owned()),
        })
        .unwrap();

        assert_eq!(
            redactor.text("rootless as root on pc in /root/x"),
            "rootless as root on pc in ~/x"
        );
    }

    #[test]
    fn replaces_secret_json_values_whatever_their_shape() {
        let redactor = redactor();
        let settings = json!({
            "proxy": {"http": "http://jdoe:pw@proxy:3128", "noProxy": "localhost"},
            "auths": {"ghcr.io": {"auth": "amRvZTpwdw==", "identitytoken": {"nested": 1}}},
            "credsStore": "desktop",
            "daemonJson": {"registry-mirrors": ["https://mirror.corp"]},
            "clientSecret": null,
            "path": "/home/jdoe/.kube/config",
        });

        assert_eq!(
            redactor.json(settings),
            json!({
                "proxy": {"http": "http://<redacted>@proxy:3128", "noProxy": "localhost"},
                "auths": {"ghcr.io": {"auth": "<redacted>", "identitytoken": "<redacted>"}},
                "credsStore": "desktop",
                "daemonJson": {"registry-mirrors": ["https://mirror.corp"]},
                "clientSecret": null,
                "path": "~/.kube/config",
            })
        );
    }
}
