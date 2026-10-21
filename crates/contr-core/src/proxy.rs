//! Proxy servers in the form the engine and most tools take them: `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY`.

/// Where outgoing connections go; a destination scheme without a server connects directly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Proxy {
    /// Proxy URL for `http://` destinations.
    pub http: Option<String>,
    /// Proxy URL for `https://` destinations.
    pub https: Option<String>,
    /// Hosts, domains (`.example.com`), IP addresses and CIDR ranges reached directly.
    pub exclude: Vec<String>,
}

impl Proxy {
    /// Whether every destination is reached directly.
    pub fn is_direct(&self) -> bool {
        self.http.is_none() && self.https.is_none()
    }
}

/// `server` as a URL; one that names no scheme, as `proxy.example.com:3128`, speaks HTTP.
pub fn server_url(server: &str) -> String {
    let server = server.trim();
    if server.contains("://") {
        server.to_owned()
    } else {
        format!("http://{server}")
    }
}

/// Entries of a `NO_PROXY` list, separated by commas, semicolons or white space.
pub fn exclusions(list: &str) -> Vec<String> {
    list.split([',', ';', ' ', '\t', '\r', '\n'])
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_without_a_scheme_speak_http() {
        assert_eq!(server_url(" proxy.example.com:3128 "), "http://proxy.example.com:3128");
        assert_eq!(server_url("socks5://127.0.0.1:1080"), "socks5://127.0.0.1:1080");
    }

    #[test]
    fn splits_exclusion_lists() {
        assert_eq!(
            exclusions(".corp.example.com, 10.0.0.0/8;localhost\n"),
            [".corp.example.com", "10.0.0.0/8", "localhost"]
        );
    }
}
