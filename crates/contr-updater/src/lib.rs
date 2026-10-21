//! Whether a newer Contr is published: reads the GitHub releases of the core through the user's proxy and picks the
//! newest version this build accepts.

use std::time::Duration;

use contr_core::product;
use contr_core::proxy::Proxy;
use semver::Version;
use serde::Deserialize;

const TIMEOUT: Duration = Duration::from_secs(30);
/// Releases one request reads; older ones cannot be the newest.
const RELEASES_PER_REQUEST: &str = "30";
const GITHUB_JSON: &str = "application/vnd.github+json";

/// A published release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// Release page.
    pub url: String,
    /// When it was published, in RFC 3339; empty when the feed did not say.
    pub published_at: String,
    /// Release notes in Markdown.
    pub notes: String,
}

/// Failure of an update check; the cause is in [`std::error::Error::source`].
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("the proxy {server} is unusable")]
    Proxy {
        server: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("cannot read the published releases")]
    Feed(#[from] reqwest::Error),
}

/// Where releases are published, and the client that reaches it.
pub struct Feed {
    url: String,
    client: reqwest::Client,
}

impl Feed {
    /// The GitHub releases endpoint `url`, reached through `proxy`.
    pub fn new(url: impl Into<String>, proxy: &Proxy) -> Result<Self, UpdateError> {
        // reqwest takes the process's TLS provider; a second install fails harmlessly.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let builder = reqwest::Client::builder()
            .user_agent(format!("{}/{}", product::DAEMON_BINARY, product::VERSION))
            .timeout(TIMEOUT)
            .no_proxy();

        Ok(Self {
            url: url.into(),
            client: with_proxy(builder, proxy)?.build()?,
        })
    }

    /// The newest release after `current`, or `None` when `current` is the newest. Prereleases count when
    /// `prereleases` is set or `current` is one itself.
    pub async fn newer_than(&self, current: &Version, prereleases: bool) -> Result<Option<Release>, UpdateError> {
        let releases: Vec<GitHubRelease> = self
            .client
            .get(format!("{}?per_page={RELEASES_PER_REQUEST}", self.url))
            .header(reqwest::header::ACCEPT, GITHUB_JSON)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(newest(releases, current, prereleases))
    }
}

fn with_proxy(mut builder: reqwest::ClientBuilder, proxy: &Proxy) -> Result<reqwest::ClientBuilder, UpdateError> {
    let exclude = reqwest::NoProxy::from_string(&proxy.exclude.join(","));
    if let Some(server) = &proxy.http {
        let http = reqwest::Proxy::http(server).map_err(|source| unusable(server, source))?;
        builder = builder.proxy(http.no_proxy(exclude.clone()));
    }
    if let Some(server) = &proxy.https {
        let https = reqwest::Proxy::https(server).map_err(|source| unusable(server, source))?;
        builder = builder.proxy(https.no_proxy(exclude));
    }

    Ok(builder)
}

fn unusable(server: &str, source: reqwest::Error) -> UpdateError {
    UpdateError::Proxy {
        server: server.to_owned(),
        source,
    }
}

/// A release as the GitHub API lists it.
#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
}

fn newest(releases: Vec<GitHubRelease>, current: &Version, prereleases: bool) -> Option<Release> {
    let accepts_prereleases = prereleases || !current.pre.is_empty();
    releases
        .into_iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let version = Version::parse(release.tag_name.trim_start_matches('v')).ok()?;
            (accepts_prereleases || version.pre.is_empty()).then(|| Release {
                version,
                url: release.html_url,
                published_at: release.published_at.unwrap_or_default(),
                notes: release.body.unwrap_or_default(),
            })
        })
        .filter(|release| release.version > *current)
        .max_by(|a, b| a.version.cmp(&b.version))
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    const RELEASES: &str = r#"[
        {"tag_name": "v0.3.0", "html_url": "https://example/0.3.0", "draft": true},
        {"tag_name": "v0.2.1-beta.1", "html_url": "https://example/0.2.1-beta.1", "published_at": "2026-12-01T00:00:00Z"},
        {"tag_name": "v0.2.0", "html_url": "https://example/0.2.0", "published_at": "2026-11-01T00:00:00Z", "body": "Notes"},
        {"tag_name": "nightly", "html_url": "https://example/nightly"},
        {"tag_name": "v0.1.0", "html_url": "https://example/0.1.0"},
        {"tag_name": "v0.1.0-alpha.2", "html_url": "https://example/0.1.0-alpha.2"}
    ]"#;

    fn releases() -> Vec<GitHubRelease> {
        serde_json::from_str(RELEASES).unwrap()
    }

    fn newest_after(current: &str, prereleases: bool) -> Option<String> {
        newest(releases(), &Version::parse(current).unwrap(), prereleases).map(|release| release.version.to_string())
    }

    #[test]
    fn picks_the_newest_published_version_after_the_current_one() {
        assert_eq!(newest_after("0.1.0", false).as_deref(), Some("0.2.0"));
        assert_eq!(newest_after("0.1.0", true).as_deref(), Some("0.2.1-beta.1"));
        assert_eq!(newest_after("0.1.0-alpha.1", false).as_deref(), Some("0.2.1-beta.1"));
        assert_eq!(newest_after("0.2.1-beta.1", false), None);
        assert_eq!(newest_after("0.2.0", false), None);
    }

    #[test]
    fn keeps_the_page_date_and_notes() {
        let release = newest(releases(), &Version::new(0, 1, 0), false).unwrap();

        assert_eq!(release.url, "https://example/0.2.0");
        assert_eq!(release.published_at, "2026-11-01T00:00:00Z");
        assert_eq!(release.notes, "Notes");
    }

    #[test]
    fn refuses_a_proxy_that_is_not_a_url() {
        let proxy = Proxy {
            https: Some("http://[::1".to_owned()),
            ..Proxy::default()
        };

        assert!(matches!(
            Feed::new("http://127.0.0.1:1/", &proxy),
            Err(UpdateError::Proxy { .. })
        ));
    }

    #[tokio::test]
    async fn reads_the_feed_over_http() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let read = stream.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{RELEASES}",
                RELEASES.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request[..read]).into_owned()
        });

        let feed = Feed::new(format!("http://{address}/releases"), &Proxy::default()).unwrap();
        let release = feed.newer_than(&Version::new(0, 1, 0), false).await.unwrap();

        assert_eq!(release.map(|release| release.version), Some(Version::new(0, 2, 0)));
        let request = server.await.unwrap();
        assert!(request.starts_with("GET /releases?per_page=30 HTTP/1.1"), "{request}");
        assert!(
            request.contains(&format!("user-agent: contrd/{}", product::VERSION)),
            "{request}"
        );
    }
}
