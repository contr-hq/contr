use http::Method;
use serde_json::{Map, Value};

const HOST_ALIAS: &str = "host.docker.internal";
const HOST_ALIAS_ENTRY: &str = "host.docker.internal:host-gateway";

/// How container definitions from Windows clients are adjusted for an engine in a Linux guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rewrites {
    /// Guest directory holding one mount per Windows drive, such as `/mnt` for `/mnt/c`.
    pub drives_root: String,
    /// Maps `host.docker.internal` to the engine's host gateway in containers that do not name it themselves.
    pub host_alias: bool,
}

/// Whether the request creates a container, the only request whose body is rewritten.
pub(crate) fn targets(method: &Method, path: &str) -> bool {
    method == Method::POST && unversioned(path) == "/containers/create"
}

/// `path` without its `/v1.52`-style API version prefix.
fn unversioned(path: &str) -> &str {
    let Some(after) = path.strip_prefix("/v") else {
        return path;
    };
    match after.find('/') {
        Some(slash) if slash > 0 && after[..slash].bytes().all(|byte| byte.is_ascii_digit() || byte == b'.') => {
            &after[slash..]
        }
        _ => path,
    }
}

/// Rewritten container definition, or `None` when `body` needs no change or is not a JSON object.
pub(crate) fn container_create(body: &[u8], rewrites: &Rewrites) -> Option<Vec<u8>> {
    let mut definition: Value = serde_json::from_slice(body).ok()?;
    let host_config = definition
        .as_object_mut()?
        .entry("HostConfig")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()?;

    let mut changed = rewrite_binds(host_config, &rewrites.drives_root);
    changed |= rewrite_mounts(host_config, &rewrites.drives_root);
    if rewrites.host_alias {
        changed |= add_host_alias(host_config);
    }

    changed.then(|| serde_json::to_vec(&definition).ok()).flatten()
}

fn rewrite_binds(host_config: &mut Map<String, Value>, drives_root: &str) -> bool {
    let Some(Value::Array(binds)) = host_config.get_mut("Binds") else {
        return false;
    };
    let mut changed = false;
    for bind in binds {
        if let Some(converted) = bind.as_str().and_then(|bind| windows_bind(bind, drives_root)) {
            *bind = Value::String(converted);
            changed = true;
        }
    }

    changed
}

fn rewrite_mounts(host_config: &mut Map<String, Value>, drives_root: &str) -> bool {
    let Some(Value::Array(mounts)) = host_config.get_mut("Mounts") else {
        return false;
    };
    let mut changed = false;
    for mount in mounts.iter_mut().filter_map(Value::as_object_mut) {
        if mount.get("Type").and_then(Value::as_str) != Some("bind") {
            continue;
        }
        let converted = mount
            .get("Source")
            .and_then(Value::as_str)
            .and_then(|source| windows_path(source, drives_root));
        if let Some(converted) = converted {
            mount.insert("Source".to_owned(), Value::String(converted));
            changed = true;
        }
    }

    changed
}

/// Adds the alias unless the container defines it or shares another container's network, where the engine refuses
/// extra hosts.
fn add_host_alias(host_config: &mut Map<String, Value>) -> bool {
    let shares_network = host_config
        .get("NetworkMode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode.starts_with("container:"));
    if shares_network {
        return false;
    }

    let extra_hosts = host_config
        .entry("ExtraHosts")
        .and_modify(|hosts| {
            if hosts.is_null() {
                *hosts = Value::Array(Vec::new());
            }
        })
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(extra_hosts) = extra_hosts.as_array_mut() else {
        return false;
    };
    let named = extra_hosts
        .iter()
        .filter_map(Value::as_str)
        .any(|entry| entry.split(':').next() == Some(HOST_ALIAS));
    if named {
        return false;
    }

    extra_hosts.push(Value::String(HOST_ALIAS_ENTRY.to_owned()));
    true
}

/// Guest path of a Windows absolute path such as `C:\src` or `c:/src`.
fn windows_path(path: &str, drives_root: &str) -> Option<String> {
    let mut chars = path.chars();
    let drive = chars.next().filter(char::is_ascii_alphabetic)?;
    if chars.next() != Some(':') {
        return None;
    }
    let rest = chars.as_str();
    if !(rest.is_empty() || rest.starts_with(['\\', '/'])) {
        return None;
    }

    Some(format!(
        "{}/{}{}",
        drives_root.trim_end_matches('/'),
        drive.to_ascii_lowercase(),
        rest.replace('\\', "/")
    ))
}

/// Guest form of a `-v` bind such as `C:\src:/w:ro`; the source ends at the first colon after the drive letter.
fn windows_bind(bind: &str, drives_root: &str) -> Option<String> {
    let source_end = bind.get(2..)?.find(':')? + 2;
    let source = windows_path(&bind[..source_end], drives_root)?;

    Some(format!("{source}{}", &bind[source_end..]))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn rewrites(host_alias: bool) -> Rewrites {
        Rewrites {
            drives_root: "/mnt".to_owned(),
            host_alias,
        }
    }

    fn rewritten(definition: &Value, host_alias: bool) -> Option<Value> {
        let body = serde_json::to_vec(definition).expect("definition serializes");
        container_create(&body, &rewrites(host_alias)).map(|bytes| serde_json::from_slice(&bytes).expect("valid JSON"))
    }

    #[test]
    fn targets_only_container_creation() {
        assert!(targets(&Method::POST, "/containers/create"));
        assert!(targets(&Method::POST, "/v1.52/containers/create"));
        assert!(!targets(&Method::GET, "/v1.52/containers/create"));
        assert!(!targets(&Method::POST, "/v1.52/containers/abc/start"));
        assert!(!targets(&Method::POST, "/vx/containers/create"));
    }

    #[test]
    fn converts_windows_paths() {
        assert_eq!(
            windows_path(r"C:\Users\dev\src", "/mnt").as_deref(),
            Some("/mnt/c/Users/dev/src")
        );
        assert_eq!(windows_path("d:/data", "/mnt/").as_deref(), Some("/mnt/d/data"));
        assert_eq!(windows_path("C:", "/mnt").as_deref(), Some("/mnt/c"));
        assert_eq!(windows_path("/home/dev", "/mnt"), None);
        assert_eq!(windows_path("CC:/x", "/mnt"), None);
        assert_eq!(windows_path("C:relative", "/mnt"), None);
    }

    #[test]
    fn converts_binds_and_keeps_options() {
        assert_eq!(windows_bind(r"C:\src:/w", "/mnt").as_deref(), Some("/mnt/c/src:/w"));
        assert_eq!(
            windows_bind(r"C:\My Files:/w:ro", "/mnt").as_deref(),
            Some("/mnt/c/My Files:/w:ro")
        );
        assert_eq!(windows_bind("named:/data", "/mnt"), None);
        assert_eq!(windows_bind("/mnt/c/src:/w", "/mnt"), None);
        assert_eq!(windows_bind(r"C:\src", "/mnt"), None);
    }

    #[test]
    fn rewrites_binds_and_bind_mounts_only() {
        let definition = json!({
            "Image": "alpine",
            "HostConfig": {
                "Binds": [r"C:\src:/w", "cache:/cache"],
                "Mounts": [
                    {"Type": "bind", "Source": r"D:\data", "Target": "/data"},
                    {"Type": "volume", "Source": "C:fake", "Target": "/v"}
                ]
            }
        });

        let result = rewritten(&definition, false).expect("definition changes");

        assert_eq!(result["HostConfig"]["Binds"], json!(["/mnt/c/src:/w", "cache:/cache"]));
        assert_eq!(result["HostConfig"]["Mounts"][0]["Source"], "/mnt/d/data");
        assert_eq!(result["HostConfig"]["Mounts"][1]["Source"], "C:fake");
        assert_eq!(result["Image"], "alpine");
    }

    #[test]
    fn adds_the_host_alias_once() {
        let first =
            rewritten(&json!({"Image": "alpine", "HostConfig": {"ExtraHosts": null}}), true).expect("alias is added");
        assert_eq!(first["HostConfig"]["ExtraHosts"], json!([HOST_ALIAS_ENTRY]));

        assert_eq!(rewritten(&first, true), None);
        let custom = json!({"HostConfig": {"ExtraHosts": ["host.docker.internal:10.0.0.1"]}});
        assert_eq!(rewritten(&custom, true), None);
    }

    #[test]
    fn creates_host_config_for_the_alias() {
        let result = rewritten(&json!({"Image": "alpine"}), true).expect("alias is added");

        assert_eq!(result["HostConfig"]["ExtraHosts"], json!([HOST_ALIAS_ENTRY]));
    }

    #[test]
    fn leaves_shared_network_containers_alone() {
        let definition = json!({"HostConfig": {"NetworkMode": "container:web"}});

        assert_eq!(rewritten(&definition, true), None);
    }

    #[test]
    fn ignores_bodies_that_are_not_objects() {
        assert_eq!(container_create(b"[1, 2]", &rewrites(true)), None);
        assert_eq!(container_create(b"not json", &rewrites(true)), None);
    }
}
