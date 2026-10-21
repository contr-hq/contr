//! Checks for a newer Contr once a day while the settings allow it, and keeps the latest result.

use std::error::Error;
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};

use contr_api::updates::v1::{Release, UpdateStatus};
use contr_core::product;
use contr_core::proxy::Proxy;
use contr_settings::{Configured, ProxyMode};
use contr_updater::Feed;
use semver::Version;
use tokio::sync::{Mutex, watch};
use tonic::Status;

use crate::settings::SettingsHub;

/// Delay of the first check after the daemon starts, so a check never slows a login.
const FIRST_CHECK: Duration = Duration::from_secs(120);
const CHECK_INTERVAL: Duration = Duration::from_hours(24);

pub(crate) struct UpdatesHub {
    /// GitHub releases endpoint.
    feed: String,
    settings: Arc<SettingsHub>,
    status: watch::Sender<UpdateStatus>,
    checking: Mutex<()>,
}

impl UpdatesHub {
    /// Checks `feed` once a day while `settings` allow it.
    pub(crate) fn spawn(settings: Arc<SettingsHub>, feed: String) -> Arc<Self> {
        let hub = Arc::new(Self {
            feed,
            settings,
            status: watch::Sender::new(UpdateStatus {
                current_version: product::VERSION.to_owned(),
                ..UpdateStatus::default()
            }),
            checking: Mutex::new(()),
        });
        tokio::spawn(check_daily(Arc::downgrade(&hub)));
        hub
    }

    pub(crate) fn status(&self) -> UpdateStatus {
        self.status.borrow().clone()
    }

    /// Checks now; fails with `FAILED_PRECONDITION` while the settings turn checks off.
    pub(crate) async fn check(&self) -> Result<UpdateStatus, Status> {
        if !self.enabled() {
            return Err(Status::failed_precondition("update checks are turned off"));
        }
        Ok(self.check_now().await)
    }

    fn enabled(&self) -> bool {
        self.settings.snapshot().effective.configured.updates.check
    }

    async fn check_now(&self) -> UpdateStatus {
        let _one_at_a_time = self.checking.lock().await;
        let outcome = newer_release(&self.feed, &self.settings.snapshot().effective.configured).await;

        let mut status = self.status();
        status.checked_at = Some(SystemTime::now().into());
        match outcome {
            Ok(available) => {
                status.available = available.map(release);
                status.error.clear();
            }
            Err(error) => {
                tracing::warn!(%error, "cannot check for updates");
                status.error = error;
            }
        }
        self.status.send_replace(status.clone());
        status
    }
}

async fn check_daily(hub: Weak<UpdatesHub>) {
    tokio::time::sleep(FIRST_CHECK).await;
    let mut interval = tokio::time::interval(CHECK_INTERVAL);
    loop {
        interval.tick().await;
        let Some(hub) = hub.upgrade() else {
            return;
        };
        if hub.enabled() {
            hub.check_now().await;
        }
    }
}

/// The newest release after this build, through the proxy the settings choose.
async fn newer_release(feed: &str, configured: &Configured) -> Result<Option<contr_updater::Release>, String> {
    let current = Version::parse(product::VERSION)
        .map_err(|error| format!("this build's version {} is not a version: {error}", product::VERSION))?;
    let proxy = match configured.proxy.mode {
        ProxyMode::Manual => configured.proxy.manual(),
        ProxyMode::None => Proxy::default(),
        ProxyMode::System => tokio::task::spawn_blocking(contr_platform::proxy::system)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default(),
    };

    let feed = Feed::new(feed, &proxy).map_err(|error| describe(&error))?;
    feed.newer_than(&current, configured.updates.prereleases)
        .await
        .map_err(|error| describe(&error))
}

/// `error` and every error that caused it, since HTTP clients name the actual cause only in their sources.
fn describe(error: &dyn Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn release(release: contr_updater::Release) -> Release {
    Release {
        version: release.version.to_string(),
        url: release.url,
        published_at: release.published_at.parse().ok(),
        notes: release.notes,
    }
}
