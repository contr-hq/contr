//! What the engine in the runtime distribution uses besides its defaults: proxies, the host's root certificates and
//! registry mirrors.

use contr_core::guest::{self, EngineConfig};
use contr_core::proxy::Proxy;
use serde_json::{Map, Value, json};

/// Destinations the engine reaches directly whatever the proxy settings say: its own loopback interface and the host.
const ALWAYS_DIRECT: [&str; 4] = ["localhost", "127.0.0.0/8", "::1", "host.docker.internal"];

/// Looks up what `settings` leave to the system and composes the engine's configuration. A lookup that fails is
/// logged, and the engine starts without what it would have added.
#[cfg(windows)]
pub(crate) async fn resolve(settings: &contr_backend::EngineSettings) -> EngineConfig {
    use std::time::Duration;

    use contr_backend::ProxyChoice;
    use tokio::task::spawn_blocking;
    use tokio::time::timeout;

    /// A configuration script or automatic detection may wait on the network.
    const PROXY_LOOKUP: Duration = Duration::from_secs(20);

    let proxy = match &settings.proxy {
        ProxyChoice::System => match timeout(PROXY_LOOKUP, spawn_blocking(contr_platform::proxy::system)).await {
            Ok(Ok(Ok(proxy))) => proxy,
            Ok(Ok(Err(error))) => {
                tracing::warn!(%error, "cannot read the system proxy; the engine connects directly");
                Proxy::default()
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "the system proxy lookup failed; the engine connects directly");
                Proxy::default()
            }
            Err(_) => {
                tracing::warn!("the system proxy lookup timed out; the engine connects directly");
                Proxy::default()
            }
        },
        ProxyChoice::Manual(proxy) => proxy.clone(),
        ProxyChoice::Direct => Proxy::default(),
    };
    let certificates = if settings.trust_system_certificates {
        match spawn_blocking(contr_platform::certificates::system_roots).await {
            Ok(Ok(roots)) => contr_platform::certificates::pem(&roots),
            Ok(Err(error)) => {
                tracing::warn!(%error, "cannot read the Windows root certificates");
                String::new()
            }
            Err(error) => {
                tracing::warn!(%error, "reading the Windows root certificates failed");
                String::new()
            }
        }
    } else {
        String::new()
    };
    tracing::info!(
        http = proxy.http.as_deref().unwrap_or("direct"),
        https = proxy.https.as_deref().unwrap_or("direct"),
        mirrors = settings.registry_mirrors.len(),
        "configuring the engine"
    );

    compose(&settings.daemon, &proxy, &settings.registry_mirrors, certificates)
}

/// The engine's configuration: the user's `daemon` with `proxy` and `mirrors` added, and the PEM `certificates`.
pub(crate) fn compose(
    daemon: &Map<String, Value>,
    proxy: &Proxy,
    mirrors: &[String],
    certificates: String,
) -> EngineConfig {
    let mut daemon = daemon.clone();
    if !proxy.is_direct() {
        let mut proxies = Map::new();
        if let Some(http) = &proxy.http {
            proxies.insert("http-proxy".to_owned(), json!(reachable(http)));
        }
        if let Some(https) = &proxy.https {
            proxies.insert("https-proxy".to_owned(), json!(reachable(https)));
        }
        let host = guest::HOST_ADDRESS.to_string();
        let direct: Vec<&str> = proxy
            .exclude
            .iter()
            .map(String::as_str)
            .chain(ALWAYS_DIRECT)
            .chain([host.as_str()])
            .collect();
        proxies.insert("no-proxy".to_owned(), json!(direct.join(",")));
        daemon.insert("proxies".to_owned(), Value::Object(proxies));
    }
    if !mirrors.is_empty() {
        daemon.insert("registry-mirrors".to_owned(), json!(mirrors));
    }

    EngineConfig { daemon, certificates }
}

/// `url` as the guest reaches it: a server on the host's loopback interface, such as a local authenticating proxy,
/// moves to [`guest::HOST_ADDRESS`], which the agent relays there.
fn reachable(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    let (user, server) = match authority.rsplit_once('@') {
        Some((user, server)) => (Some(user), server),
        None => (None, authority),
    };
    let (host, port) = match server.strip_prefix('[') {
        Some(bracketed) => match bracketed.split_once(']') {
            Some((host, port)) => (host, port),
            None => return url.to_owned(),
        },
        None => server.split_at(server.find(':').unwrap_or(server.len())),
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if !loopback {
        return url.to_owned();
    }

    let user = user.map(|user| format!("{user}@")).unwrap_or_default();
    format!("{scheme}://{user}{}{port}{path}", guest::HOST_ADDRESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_direct_engine_gets_the_users_settings_and_mirrors() {
        let daemon = json!({"log-level": "warn"}).as_object().cloned().unwrap();

        let config = compose(
            &daemon,
            &Proxy::default(),
            &["https://mirror.corp".to_owned()],
            String::new(),
        );

        assert_eq!(
            Value::Object(config.daemon),
            json!({"log-level": "warn", "registry-mirrors": ["https://mirror.corp"]})
        );
    }

    #[test]
    fn proxies_keep_loopback_and_the_host_direct() {
        let proxy = Proxy {
            http: Some("http://proxy.corp:3128".to_owned()),
            https: Some("http://127.0.0.1:3129".to_owned()),
            exclude: vec![".corp".to_owned()],
        };

        let config = compose(&Map::new(), &proxy, &[], "PEM".to_owned());

        assert_eq!(
            Value::Object(config.daemon),
            json!({"proxies": {
                "http-proxy": "http://proxy.corp:3128",
                "https-proxy": "http://198.19.255.254:3129",
                "no-proxy": ".corp,localhost,127.0.0.0/8,::1,host.docker.internal,198.19.255.254",
            }})
        );
        assert_eq!(config.certificates, "PEM");
    }

    #[test]
    fn moves_only_loopback_servers_to_the_host_address() {
        assert_eq!(reachable("http://localhost:3128"), "http://198.19.255.254:3128");
        assert_eq!(
            reachable("http://me:pw@[::1]:3128/"),
            "http://me:pw@198.19.255.254:3128/"
        );
        assert_eq!(reachable("socks5://127.0.0.2"), "socks5://198.19.255.254");
        assert_eq!(reachable("http://proxy.corp:3128"), "http://proxy.corp:3128");
        assert_eq!(reachable("http://10.0.0.1:80"), "http://10.0.0.1:80");
    }
}
