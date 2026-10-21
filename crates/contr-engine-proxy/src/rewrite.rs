use std::borrow::Cow;

use contr_core::proxy::Proxy;
use http::Method;
use serde_json::{Map, Value};

use crate::variables::ProxyVariables;

const HOST_ALIAS: &str = "host.docker.internal";
const HOST_ALIAS_ENTRY: &str = "host.docker.internal:host-gateway";

/// Directory under which WSL mounts the Windows drives and its shared `wsl` directory in every distribution.
const SHARED_MOUNTS: &str = "/mnt";
/// Sources that name the engine socket, which means the same in the client's distribution and in the guest.
const ENGINE_SOCKETS: [&str; 2] = ["/var/run/docker.sock", "/run/docker.sock"];
/// Query parameter of `/build` that holds the build arguments as a JSON object.
const BUILD_ARGS: &str = "buildargs";

/// How container definitions from clients outside the engine's Linux guest are adjusted for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rewrites {
    /// Guest directory holding one mount per Windows drive, such as `/mnt` for `/mnt/c`.
    pub drives_root: String,
    /// Guest mount of the root file system of the WSL distribution the client runs in. Absolute Linux sources
    /// move under it unless they lie in `/mnt` or name the engine socket; submounts of that distribution are not
    /// visible there. `None` for Windows clients.
    pub distro_root: Option<String>,
    /// Maps `host.docker.internal` to the engine's host gateway in containers that do not name it themselves.
    pub host_alias: bool,
    /// Proxy that new containers and builds receive as `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY` unless they set
    /// the variable themselves; its servers must be reachable from the guest.
    pub proxy: Option<Proxy>,
}

impl Rewrites {
    pub(crate) fn proxy_variables(&self) -> Option<ProxyVariables> {
        self.proxy.as_ref().and_then(ProxyVariables::of)
    }
}

/// Requests whose content the proxy may change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    ContainerCreate,
    /// The classic build endpoint, whose query carries the build arguments.
    Build,
    /// BuildKit's gRPC API, which the client reaches by upgrading the connection to HTTP/2.
    BuildKit,
}

pub(crate) fn target(method: &Method, path: &str) -> Option<Target> {
    if method != Method::POST {
        return None;
    }

    match unversioned(path) {
        "/containers/create" => Some(Target::ContainerCreate),
        "/build" => Some(Target::Build),
        "/grpc" => Some(Target::BuildKit),
        _ => None,
    }
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
    let fields = definition.as_object_mut()?;
    let mut changed = match rewrites.proxy_variables() {
        Some(variables) => add_variables(fields, &variables),
        None => false,
    };
    let host_config = fields
        .entry("HostConfig")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()?;

    changed |= rewrite_binds(host_config, rewrites);
    changed |= rewrite_mounts(host_config, rewrites);
    if rewrites.host_alias {
        changed |= add_host_alias(host_config);
    }

    changed.then(|| serde_json::to_vec(&definition).ok()).flatten()
}

/// Appends the proxy variables the definition's `Env` does not set.
fn add_variables(definition: &mut Map<String, Value>, variables: &ProxyVariables) -> bool {
    let env = definition
        .entry("Env")
        .and_modify(|env| {
            if env.is_null() {
                *env = Value::Array(Vec::new());
            }
        })
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(env) = env.as_array_mut() else {
        return false;
    };
    let defined: Vec<&str> = env
        .iter()
        .filter_map(Value::as_str)
        .map(|entry| entry.split_once('=').map_or(entry, |(name, _)| name))
        .collect();
    let missing: Vec<Value> = variables
        .missing(&defined)
        .into_iter()
        .map(|(name, value)| Value::String(format!("{name}={value}")))
        .collect();
    if missing.is_empty() {
        return false;
    }

    env.extend(missing);
    true
}

/// Query of a `/build` request with the proxy variables its build arguments do not set added to them, or `None` when
/// it needs no change or its build arguments are not a JSON object. Every other parameter keeps its encoding.
pub(crate) fn build_query(query: &str, variables: &ProxyVariables) -> Option<String> {
    let mut pieces: Vec<&str> = query.split('&').filter(|piece| !piece.is_empty()).collect();
    let position = pieces
        .iter()
        .position(|piece| parameter(piece).is_some_and(|(name, _)| name == BUILD_ARGS));
    let mut args = match position.and_then(|index| parameter(pieces[index])) {
        Some((_, value)) => match serde_json::from_str(&value).ok()? {
            Value::Object(args) => args,
            Value::Null => Map::new(),
            _ => return None,
        },
        None => Map::new(),
    };
    let defined: Vec<&str> = args.keys().map(String::as_str).collect();
    let missing: Vec<(String, Value)> = variables
        .missing(&defined)
        .into_iter()
        .map(|(name, value)| (name, Value::String(value.to_owned())))
        .collect();
    if missing.is_empty() {
        return None;
    }
    args.extend(missing);

    let encoded: String = form_urlencoded::byte_serialize(Value::Object(args).to_string().as_bytes()).collect();
    let replacement = format!("{BUILD_ARGS}={encoded}");
    match position {
        Some(index) => pieces[index] = &replacement,
        None => pieces.push(&replacement),
    }
    Some(pieces.join("&"))
}

fn rewrite_binds(host_config: &mut Map<String, Value>, rewrites: &Rewrites) -> bool {
    let Some(Value::Array(binds)) = host_config.get_mut("Binds") else {
        return false;
    };
    let mut changed = false;
    for bind in binds {
        if let Some(converted) = bind.as_str().and_then(|bind| guest_bind(bind, rewrites)) {
            *bind = Value::String(converted);
            changed = true;
        }
    }

    changed
}

fn rewrite_mounts(host_config: &mut Map<String, Value>, rewrites: &Rewrites) -> bool {
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
            .and_then(|source| guest_path(source, rewrites));
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

fn parameter(piece: &str) -> Option<(Cow<'_, str>, Cow<'_, str>)> {
    form_urlencoded::parse(piece.as_bytes()).next()
}

/// Guest path of a bind source, or `None` when the guest reads it as written.
fn guest_path(source: &str, rewrites: &Rewrites) -> Option<String> {
    windows_path(source, &rewrites.drives_root).or_else(|| distro_path(source, rewrites.distro_root.as_deref()?))
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

/// Guest path of an absolute path in the client's distribution.
fn distro_path(path: &str, distro_root: &str) -> Option<String> {
    let shared = path == SHARED_MOUNTS
        || path
            .strip_prefix(SHARED_MOUNTS)
            .is_some_and(|rest| rest.starts_with('/'))
        || ENGINE_SOCKETS.contains(&path);
    if !path.starts_with('/') || shared {
        return None;
    }

    Some(format!("{}{path}", distro_root.trim_end_matches('/')))
}

/// Guest form of a `-v` bind such as `C:\src:/w:ro` or `/src:/w`; a Windows source ends at the first colon after
/// its drive letter.
fn guest_bind(bind: &str, rewrites: &Rewrites) -> Option<String> {
    let drive_letter = bind.get(1..2) == Some(":");
    let source_end = if drive_letter {
        bind.get(2..)?.find(':')? + 2
    } else {
        bind.find(':')?
    };
    let source = guest_path(&bind[..source_end], rewrites)?;

    Some(format!("{source}{}", &bind[source_end..]))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn rewrites(host_alias: bool) -> Rewrites {
        Rewrites {
            drives_root: "/mnt".to_owned(),
            distro_root: None,
            host_alias,
            proxy: None,
        }
    }

    fn proxied() -> Rewrites {
        Rewrites {
            proxy: Some(Proxy {
                http: Some("http://198.19.255.254:3128".to_owned()),
                https: Some("http://proxy.corp:8080".to_owned()),
                exclude: vec!["localhost".to_owned(), ".corp".to_owned()],
            }),
            ..rewrites(false)
        }
    }

    fn distro_rewrites() -> Rewrites {
        Rewrites {
            distro_root: Some("/mnt/wsl/contr/distros/Ubuntu".to_owned()),
            ..rewrites(false)
        }
    }

    fn rewritten(definition: &Value, host_alias: bool) -> Option<Value> {
        let body = serde_json::to_vec(definition).expect("definition serializes");
        container_create(&body, &rewrites(host_alias)).map(|bytes| serde_json::from_slice(&bytes).expect("valid JSON"))
    }

    #[test]
    fn targets_container_creation_and_builds() {
        assert_eq!(
            target(&Method::POST, "/containers/create"),
            Some(Target::ContainerCreate)
        );
        assert_eq!(
            target(&Method::POST, "/v1.52/containers/create"),
            Some(Target::ContainerCreate)
        );
        assert_eq!(target(&Method::POST, "/v1.52/build"), Some(Target::Build));
        assert_eq!(target(&Method::POST, "/v1.52/grpc"), Some(Target::BuildKit));
        assert_eq!(target(&Method::GET, "/v1.52/containers/create"), None);
        assert_eq!(target(&Method::POST, "/v1.52/containers/abc/start"), None);
        assert_eq!(target(&Method::POST, "/vx/containers/create"), None);
        assert_eq!(target(&Method::POST, "/v1.52/session"), None);
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
        let windows = rewrites(false);

        assert_eq!(guest_bind(r"C:\src:/w", &windows).as_deref(), Some("/mnt/c/src:/w"));
        assert_eq!(
            guest_bind(r"C:\My Files:/w:ro", &windows).as_deref(),
            Some("/mnt/c/My Files:/w:ro")
        );
        assert_eq!(guest_bind("named:/data", &windows), None);
        assert_eq!(guest_bind("/mnt/c/src:/w", &windows), None);
        assert_eq!(guest_bind("/home/dev/src:/w", &windows), None);
        assert_eq!(guest_bind(r"C:\src", &windows), None);
    }

    #[test]
    fn moves_distribution_paths_under_its_root() {
        let distro = distro_rewrites();

        assert_eq!(
            guest_bind("/home/dev/src:/w:ro", &distro).as_deref(),
            Some("/mnt/wsl/contr/distros/Ubuntu/home/dev/src:/w:ro")
        );
        assert_eq!(guest_bind("/mnt/c/src:/w", &distro), None);
        assert_eq!(guest_bind("/mnt/wsl/shared:/w", &distro), None);
        assert_eq!(guest_bind("/var/run/docker.sock:/var/run/docker.sock", &distro), None);
        assert_eq!(guest_bind("named:/data", &distro), None);
        assert_eq!(
            guest_path("/mntx", &distro).as_deref(),
            Some("/mnt/wsl/contr/distros/Ubuntu/mntx")
        );
        assert_eq!(guest_path(r"D:\data", &distro).as_deref(), Some("/mnt/d/data"));
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
    fn adds_the_proxy_variables_a_container_does_not_set() {
        let definition = json!({"Image": "alpine", "Env": ["PATH=/bin", "https_proxy=http://mine:1", "NO_PROXY"]});
        let body = serde_json::to_vec(&definition).unwrap();

        let result: Value = serde_json::from_slice(&container_create(&body, &proxied()).unwrap()).unwrap();

        assert_eq!(
            result["Env"],
            json!([
                "PATH=/bin",
                "https_proxy=http://mine:1",
                "NO_PROXY",
                "HTTP_PROXY=http://198.19.255.254:3128",
                "http_proxy=http://198.19.255.254:3128",
            ])
        );
    }

    #[test]
    fn creates_the_environment_for_the_proxy_variables() {
        for definition in [json!({"Image": "alpine"}), json!({"Image": "alpine", "Env": null})] {
            let body = serde_json::to_vec(&definition).unwrap();

            let result: Value = serde_json::from_slice(&container_create(&body, &proxied()).unwrap()).unwrap();

            assert_eq!(
                result["Env"],
                json!([
                    "HTTP_PROXY=http://198.19.255.254:3128",
                    "http_proxy=http://198.19.255.254:3128",
                    "HTTPS_PROXY=http://proxy.corp:8080",
                    "https_proxy=http://proxy.corp:8080",
                    "NO_PROXY=localhost,.corp",
                    "no_proxy=localhost,.corp",
                ])
            );
        }
    }

    #[test]
    fn leaves_containers_alone_without_a_proxy() {
        let direct = Rewrites {
            proxy: Some(Proxy::default()),
            ..rewrites(false)
        };

        assert_eq!(container_create(br#"{"Image": "alpine"}"#, &direct), None);
        assert_eq!(container_create(br#"{"Image": "alpine"}"#, &rewrites(false)), None);
    }

    fn build_args(query: &str) -> Value {
        let (_, value) = form_urlencoded::parse(query.as_bytes())
            .find(|(name, _)| name == BUILD_ARGS)
            .unwrap();
        serde_json::from_str(&value).unwrap()
    }

    #[test]
    fn adds_the_proxy_to_the_build_arguments_and_keeps_other_parameters() {
        let variables = proxied().proxy_variables().unwrap();
        let query =
            "t=app%3Alatest&buildargs=%7B%22HTTP_PROXY%22%3A%22http%3A%2F%2Fmine%3A1%22%2C%22V%22%3Anull%7D&q=1";

        let rewritten = build_query(query, &variables).unwrap();

        assert!(rewritten.starts_with("t=app%3Alatest&buildargs="), "{rewritten}");
        assert!(rewritten.ends_with("&q=1"), "{rewritten}");
        assert_eq!(
            build_args(&rewritten),
            json!({
                "HTTP_PROXY": "http://mine:1",
                "V": null,
                "HTTPS_PROXY": "http://proxy.corp:8080",
                "https_proxy": "http://proxy.corp:8080",
                "NO_PROXY": "localhost,.corp",
                "no_proxy": "localhost,.corp",
            })
        );
    }

    #[test]
    fn adds_build_arguments_to_builds_without_them() {
        let variables = proxied().proxy_variables().unwrap();

        let rewritten = build_query("", &variables).unwrap();

        assert_eq!(build_args(&rewritten)["http_proxy"], "http://198.19.255.254:3128");
        assert_eq!(build_query("buildargs=%5B%5D", &variables), None);
        assert_eq!(build_query("buildargs=nope", &variables), None);
    }

    #[test]
    fn ignores_bodies_that_are_not_objects() {
        assert_eq!(container_create(b"[1, 2]", &rewrites(true)), None);
        assert_eq!(container_create(b"not json", &rewrites(true)), None);
    }
}
