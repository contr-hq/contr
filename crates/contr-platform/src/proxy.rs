//! The current user's proxy for outgoing connections.

use std::io;
#[cfg(any(windows, test))]
use std::net::Ipv4Addr;

use contr_core::proxy::{Proxy, exclusions, server_url};

/// URL a proxy configuration script is asked about; the engine's pulls go to Docker Hub unless told otherwise.
#[cfg(windows)]
const PROBE_URL: &str = "https://registry-1.docker.io/v2/";

/// The current user's proxy. On Windows that is the proxy settings, where a configuration script or automatic
/// detection is asked about Docker Hub; when they name no proxy, or elsewhere, it is `HTTP_PROXY`, `HTTPS_PROXY` and
/// `NO_PROXY`. Blocks while a script is fetched or detected.
pub fn system() -> io::Result<Proxy> {
    #[cfg(windows)]
    let configured = windows::system()?;
    #[cfg(not(windows))]
    let configured = Proxy::default();

    Ok(if configured.is_direct() {
        from_environment(|name| std::env::var(name).ok())
    } else {
        configured
    })
}

/// The proxy `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY` name, or their lowercase forms.
fn from_environment(var: impl Fn(&str) -> Option<String>) -> Proxy {
    let read = |name: &str| {
        var(name)
            .or_else(|| var(&name.to_lowercase()))
            .filter(|value| !value.trim().is_empty())
    };

    Proxy {
        http: read("HTTP_PROXY").map(|server| server_url(&server)),
        https: read("HTTPS_PROXY").map(|server| server_url(&server)),
        exclude: read("NO_PROXY").map(|list| exclusions(&list)).unwrap_or_default(),
    }
}

/// HTTP and HTTPS proxies of a Windows proxy server list: one server for every scheme, as `proxy:3128`, or servers per
/// scheme, as `http=proxy:3128;https=proxy:3129`. Later servers for the same scheme are fallbacks, which the engine
/// cannot use.
#[cfg(any(windows, test))]
fn servers(list: &str) -> (Option<String>, Option<String>) {
    let (mut http, mut https, mut every) = (None, None, None);
    for entry in list
        .split([';', ' ', '\t', '\r', '\n'])
        .filter(|entry| !entry.is_empty())
    {
        match entry.split_once('=') {
            Some((scheme, server)) if scheme.eq_ignore_ascii_case("http") => {
                http.get_or_insert_with(|| server_url(server));
            }
            Some((scheme, server)) if scheme.eq_ignore_ascii_case("https") => {
                https.get_or_insert_with(|| server_url(server));
            }
            Some(_) => {}
            None => {
                every.get_or_insert_with(|| server_url(entry));
            }
        }
    }

    (http.or_else(|| every.clone()), https.or(every))
}

/// `NO_PROXY` entries of a Windows proxy bypass list such as `*.corp.example.com;10.*;<local>`. `<local>`, which matches
/// names without a dot, has no equivalent and is left out.
#[cfg(any(windows, test))]
fn bypass(list: &str) -> Vec<String> {
    exclusions(list)
        .into_iter()
        .filter(|entry| !entry.eq_ignore_ascii_case("<local>"))
        .map(|entry| {
            let host = entry.split_once("://").map_or(entry.as_str(), |(_, host)| host);
            if host == "*" {
                return host.to_owned();
            }
            wildcard_range(host).unwrap_or_else(|| host.trim_start_matches('*').to_owned())
        })
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// CIDR range of an IPv4 pattern whose trailing octets are wildcards, as `10.*` or `192.168.*.*`.
#[cfg(any(windows, test))]
fn wildcard_range(pattern: &str) -> Option<String> {
    let parts: Vec<&str> = pattern.split('.').collect();
    let fixed = parts.iter().take_while(|part| **part != "*").count();
    if fixed == 0 || fixed == parts.len() || parts.len() > 4 || parts[fixed..].iter().any(|part| *part != "*") {
        return None;
    }
    let mut octets = [0u8; 4];
    for (octet, part) in octets.iter_mut().zip(&parts[..fixed]) {
        *octet = part.parse().ok()?;
    }

    Some(format!("{}/{}", Ipv4Addr::from(octets), fixed * 8))
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use contr_core::product;
    use contr_core::proxy::Proxy;
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::Networking::WinHttp::{
        WINHTTP_ACCESS_TYPE_NAMED_PROXY, WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_AUTO_DETECT_TYPE_DHCP,
        WINHTTP_AUTO_DETECT_TYPE_DNS_A, WINHTTP_AUTOPROXY_AUTO_DETECT, WINHTTP_AUTOPROXY_CONFIG_URL,
        WINHTTP_AUTOPROXY_OPTIONS, WINHTTP_CURRENT_USER_IE_PROXY_CONFIG, WINHTTP_PROXY_INFO, WinHttpCloseHandle,
        WinHttpGetIEProxyConfigForCurrentUser, WinHttpGetProxyForUrl, WinHttpOpen,
    };
    use windows_sys::core::PWSTR;

    use super::{PROBE_URL, bypass, servers};

    /// Windows' proxy settings for the current user.
    struct Settings {
        auto_detect: bool,
        script: Option<String>,
        servers: Option<String>,
        bypass: Option<String>,
    }

    enum Detected {
        Direct,
        Servers(String),
        /// Neither the script nor detection gave an answer; Windows then uses the configured servers.
        Unavailable,
    }

    pub(super) fn system() -> io::Result<Proxy> {
        let settings = settings()?;
        let detected = if settings.auto_detect || settings.script.is_some() {
            detect(settings.script.as_deref(), settings.auto_detect)
        } else {
            Detected::Unavailable
        };
        let list = match detected {
            Detected::Servers(list) => Some(list),
            Detected::Direct => None,
            Detected::Unavailable => settings.servers,
        };
        let (http, https) = list.as_deref().map(servers).unwrap_or_default();

        Ok(Proxy {
            http,
            https,
            exclude: settings.bypass.as_deref().map(bypass).unwrap_or_default(),
        })
    }

    fn settings() -> io::Result<Settings> {
        let mut config = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
        // SAFETY: `config` is a valid output location.
        if unsafe { WinHttpGetIEProxyConfigForCurrentUser(&raw mut config) } == 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: each string is null or allocated by the call above, and is taken once.
        unsafe {
            Ok(Settings {
                auto_detect: config.fAutoDetect != 0,
                script: take(config.lpszAutoConfigUrl),
                servers: take(config.lpszProxy),
                bypass: take(config.lpszProxyBypass),
            })
        }
    }

    /// The proxy that the configuration script at `script`, or the one detection finds, names for [`PROBE_URL`].
    fn detect(script: Option<&str>, auto_detect: bool) -> Detected {
        let agent = wide(product::DISPLAY_NAME);
        // SAFETY: `agent` is NUL-terminated; direct access takes no proxy or bypass list.
        let session = unsafe {
            WinHttpOpen(
                agent.as_ptr(),
                WINHTTP_ACCESS_TYPE_NO_PROXY,
                ptr::null(),
                ptr::null(),
                0,
            )
        };
        if session.is_null() {
            return Detected::Unavailable;
        }

        let script = script.map(wide);
        let mut options = WINHTTP_AUTOPROXY_OPTIONS {
            fAutoLogonIfChallenged: 1,
            ..Default::default()
        };
        if let Some(script) = &script {
            options.dwFlags |= WINHTTP_AUTOPROXY_CONFIG_URL;
            options.lpszAutoConfigUrl = script.as_ptr();
        }
        if auto_detect {
            options.dwFlags |= WINHTTP_AUTOPROXY_AUTO_DETECT;
            options.dwAutoDetectFlags = WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A;
        }
        let url = wide(PROBE_URL);
        let mut info = WINHTTP_PROXY_INFO::default();
        // SAFETY: `session` is open, the strings are NUL-terminated and outlive the call, `info` is a valid output.
        let found = unsafe { WinHttpGetProxyForUrl(session, url.as_ptr(), &raw mut options, &raw mut info) } != 0;
        // SAFETY: `session` is open and closed once.
        unsafe { WinHttpCloseHandle(session) };
        if !found {
            return Detected::Unavailable;
        }

        // SAFETY: both strings are null or allocated by the lookup above, and are taken once.
        let (servers, _) = unsafe { (take(info.lpszProxy), take(info.lpszProxyBypass)) };
        match servers {
            Some(list) if info.dwAccessType == WINHTTP_ACCESS_TYPE_NAMED_PROXY => Detected::Servers(list),
            _ => Detected::Direct,
        }
    }

    /// Text of a string Windows allocated, which this frees; `None` for null or blank text.
    ///
    /// # Safety
    ///
    /// `text` is null or a NUL-terminated string allocated with `GlobalAlloc` that nothing else frees.
    unsafe fn take(text: PWSTR) -> Option<String> {
        if text.is_null() {
            return None;
        }
        let mut len = 0;
        // SAFETY: the caller guarantees a NUL terminator, so every unit up to it is readable.
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` readable units precede the terminator.
        let value = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        // SAFETY: the caller hands over the `GlobalAlloc` allocation.
        unsafe { GlobalFree(text.cast()) };

        Some(value).filter(|value| !value.trim().is_empty())
    }

    fn wide(text: &str) -> Vec<u16> {
        OsStr::new(text).encode_wide().chain([0]).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn one_server_serves_every_scheme() {
        assert_eq!(
            servers("proxy.corp:8080"),
            (
                Some("http://proxy.corp:8080".to_owned()),
                Some("http://proxy.corp:8080".to_owned())
            )
        );
    }

    #[test]
    fn servers_per_scheme_win_and_the_first_of_a_list_is_used() {
        assert_eq!(
            servers("HTTPS=secure:3129;ftp=files:21;http=plain:3128 http=fallback:3128"),
            (
                Some("http://plain:3128".to_owned()),
                Some("http://secure:3129".to_owned())
            )
        );
        assert_eq!(servers("socks=socks:1080"), (None, None));
    }

    #[test]
    fn bypass_lists_become_no_proxy_entries() {
        assert_eq!(
            bypass("*.corp.example.com;<local>;10.*;192.168.*.*;intranet;https://*.svc;*"),
            [
                ".corp.example.com",
                "10.0.0.0/8",
                "192.168.0.0/16",
                "intranet",
                ".svc",
                "*"
            ]
        );
        assert_eq!(bypass("10.*.1.*;host.*"), ["10.*.1.*", "host.*"]);
    }

    #[test]
    fn reads_the_environment_in_either_case() {
        let vars = HashMap::from([
            ("https_proxy", "proxy:3129"),
            ("HTTP_PROXY", "http://proxy:3128"),
            ("NO_PROXY", "localhost,.corp"),
        ]);

        let proxy = from_environment(|name| vars.get(name).map(|value| (*value).to_owned()));

        assert_eq!(
            proxy,
            Proxy {
                http: Some("http://proxy:3128".to_owned()),
                https: Some("http://proxy:3129".to_owned()),
                exclude: vec!["localhost".to_owned(), ".corp".to_owned()],
            }
        );
        assert!(from_environment(|_| None).is_direct());
    }
}
