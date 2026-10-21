//! Registry credentials `docker login` stored, looked up the way the Docker CLI looks them up: the registry's
//! credential helper, else the configured credential store, else `auths` in config.json. Helpers speak the protocol of
//! docker/docker-credential-helpers (MIT): the server address on stdin, `{"Username", "Secret"}` on stdout.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

/// Key Docker Hub's credentials are stored under.
pub const DOCKER_HUB_SERVER: &str = "https://index.docker.io/v1/";
/// Registry that references without a registry host name.
const DOCKER_HUB_HOST: &str = "docker.io";
/// Username a helper returns when `Secret` is an identity token.
const TOKEN_USERNAME: &str = "<token>";
const HELPER_PREFIX: &str = "docker-credential-";

/// Credentials for one registry.
#[derive(Clone, PartialEq, Eq)]
pub struct RegistryCredentials {
    /// Address the credentials were stored under, which the engine passes on to the registry.
    pub server: String,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Refresh token that stands in for the username and password.
    pub identity_token: Option<String>,
}

impl std::fmt::Debug for RegistryCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryCredentials")
            .field("server", &self.server)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// Registry host of an image reference: its first component when that holds a dot or a colon or is `localhost`, else
/// Docker Hub.
pub fn registry_of(reference: &str) -> &str {
    match reference.split_once('/') {
        Some((first, _)) if first.contains(['.', ':']) || first == "localhost" => first,
        _ => DOCKER_HUB_HOST,
    }
}

/// Credentials for the registry of `reference` from the configuration in `config_dir`, or `None` when the user stored
/// none; helpers are found in `helper_dirs`, then on `PATH`.
pub fn for_reference(
    config_dir: &Path,
    reference: &str,
    helper_dirs: &[PathBuf],
) -> io::Result<Option<RegistryCredentials>> {
    let config = match std::fs::read(config_dir.join("config.json")) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).map_err(io::Error::other)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let host = registry_of(reference);
    let server = if host == DOCKER_HUB_HOST {
        DOCKER_HUB_SERVER
    } else {
        host
    };

    let helper = config["credHelpers"][server]
        .as_str()
        .or_else(|| config["credHelpers"][host].as_str())
        .or_else(|| config["credsStore"].as_str())
        .filter(|helper| !helper.is_empty());
    if let Some(helper) = helper {
        return from_helper(helper, server, helper_dirs);
    }

    Ok(config["auths"]
        .as_object()
        .and_then(|auths| auths.iter().find(|(key, _)| *key == server || host_of(key) == host))
        .and_then(|(key, entry)| from_auths_entry(key, entry)))
}

/// `https://registry.corp:5000/v2/` as `registry.corp:5000`.
fn host_of(server: &str) -> &str {
    let without_scheme = server.split_once("://").map_or(server, |(_, rest)| rest);
    without_scheme.split('/').next().unwrap_or_default()
}

fn from_auths_entry(server: &str, entry: &Value) -> Option<RegistryCredentials> {
    let identity_token = entry["identitytoken"].as_str().filter(|token| !token.is_empty());
    let decoded = entry["auth"]
        .as_str()
        .and_then(|auth| STANDARD.decode(auth).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok());
    let (username, password) = match decoded.as_deref().and_then(|pair| pair.split_once(':')) {
        Some((username, password)) => (Some(username.to_owned()), Some(password.to_owned())),
        None => (None, None),
    };
    if username.is_none() && identity_token.is_none() {
        return None;
    }

    Some(RegistryCredentials {
        server: server.to_owned(),
        username,
        password: password.filter(|_| identity_token.is_none()),
        identity_token: identity_token.map(str::to_owned),
    })
}

fn from_helper(helper: &str, server: &str, helper_dirs: &[PathBuf]) -> io::Result<Option<RegistryCredentials>> {
    let program = helper_program(helper, helper_dirs);
    let mut child = Command::new(&program)
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| io::Error::new(error.kind(), format!("cannot run {}: {error}", program.display())))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(server.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    // Helpers exit non-zero, saying "credentials not found", for a server they hold nothing for.
    if !output.status.success() {
        return Ok(None);
    }

    let reply: Value = serde_json::from_slice(&output.stdout).map_err(io::Error::other)?;
    let username = reply["Username"].as_str().unwrap_or_default();
    let secret = reply["Secret"].as_str().unwrap_or_default().to_owned();
    if secret.is_empty() {
        return Ok(None);
    }

    Ok(Some(if username == TOKEN_USERNAME {
        RegistryCredentials {
            server: server.to_owned(),
            username: None,
            password: None,
            identity_token: Some(secret),
        }
    } else {
        RegistryCredentials {
            server: server.to_owned(),
            username: Some(username.to_owned()),
            password: Some(secret),
            identity_token: None,
        }
    }))
}

fn helper_program(helper: &str, helper_dirs: &[PathBuf]) -> PathBuf {
    let file = format!("{HELPER_PREFIX}{helper}{}", std::env::consts::EXE_SUFFIX);
    helper_dirs
        .iter()
        .map(|dir| dir.join(&file))
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), json).unwrap();
        dir
    }

    #[test]
    fn finds_the_registry_of_a_reference() {
        assert_eq!(registry_of("nginx"), "docker.io");
        assert_eq!(registry_of("library/nginx:1.27"), "docker.io");
        assert_eq!(registry_of("ghcr.io/contr-hq/app:1"), "ghcr.io");
        assert_eq!(registry_of("registry.corp:5000/team/app"), "registry.corp:5000");
        assert_eq!(registry_of("localhost/app"), "localhost");
        assert_eq!(registry_of("localhost:5000/app"), "localhost:5000");
    }

    #[test]
    fn reads_auths_by_server_or_host() {
        let dir = config(
            r#"{"auths": {
                "https://index.docker.io/v1/": {"auth": "amRvZTpodW50ZXIy"},
                "https://registry.corp:5000/v2/": {"identitytoken": "refresh"},
                "ghcr.io": {}
            }}"#,
        );

        let hub = for_reference(dir.path(), "nginx", &[]).unwrap().unwrap();
        assert_eq!(hub.server, DOCKER_HUB_SERVER);
        assert_eq!(hub.username.as_deref(), Some("jdoe"));
        assert_eq!(hub.password.as_deref(), Some("hunter2"));
        let corp = for_reference(dir.path(), "registry.corp:5000/team/app", &[])
            .unwrap()
            .unwrap();
        assert_eq!(corp.identity_token.as_deref(), Some("refresh"));
        assert_eq!(corp.password, None);
        assert_eq!(for_reference(dir.path(), "ghcr.io/x/y", &[]).unwrap(), None);
        assert_eq!(for_reference(dir.path(), "quay.io/x/y", &[]).unwrap(), None);
    }

    #[test]
    fn without_a_config_there_are_no_credentials() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(for_reference(dir.path(), "nginx", &[]).unwrap(), None);
    }

    #[test]
    fn keeps_secrets_out_of_debug_output() {
        let credentials = RegistryCredentials {
            server: "ghcr.io".to_owned(),
            username: Some("jdoe".to_owned()),
            password: Some("hunter2".to_owned()),
            identity_token: Some("refresh".to_owned()),
        };

        let debug = format!("{credentials:?}");
        assert!(!debug.contains("hunter2") && !debug.contains("refresh"), "{debug}");
    }

    #[cfg(unix)]
    #[test]
    fn asks_the_registrys_helper_before_the_store() {
        use std::os::unix::fs::PermissionsExt;

        let helpers = tempfile::tempdir().unwrap();
        for (name, reply) in [
            (
                "corp",
                r#"{"ServerURL": "registry.corp", "Username": "svc", "Secret": "pw"}"#,
            ),
            (
                "store",
                r#"{"ServerURL": "x", "Username": "<token>", "Secret": "refresh"}"#,
            ),
        ] {
            let path = helpers.path().join(format!("docker-credential-{name}"));
            std::fs::write(&path, format!("#!/bin/sh\nread server\necho '{reply}'\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let missing = helpers.path().join("docker-credential-empty");
        std::fs::write(
            &missing,
            "#!/bin/sh\necho 'credentials not found in native keychain'\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&missing, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir = config(r#"{"credsStore": "store", "credHelpers": {"registry.corp": "corp", "quay.io": "empty"}}"#);
        let dirs = [helpers.path().to_path_buf()];

        let corp = for_reference(dir.path(), "registry.corp/app", &dirs).unwrap().unwrap();
        assert_eq!(
            (corp.username.as_deref(), corp.password.as_deref()),
            (Some("svc"), Some("pw"))
        );
        let hub = for_reference(dir.path(), "nginx", &dirs).unwrap().unwrap();
        assert_eq!(hub.server, DOCKER_HUB_SERVER);
        assert_eq!(hub.identity_token.as_deref(), Some("refresh"));
        assert_eq!(for_reference(dir.path(), "quay.io/a/b", &dirs).unwrap(), None);
    }
}
