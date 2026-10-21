//! The calling user's settings: the stored file, the administrator's policy and the effective result, which services
//! read and watch. Updates go through one lock, so each reads the file it replaces.

use std::path::PathBuf;

use contr_backend::{Backend, BackendSettings, EngineSettings, ProxyChoice};
use contr_settings::{Effective, ProxyMode, Settings, SettingsError, SettingsFile};
use serde_json::{Map, Value};
use tokio::sync::{Mutex, watch};
use tonic::Status;

/// Effective settings and a revision that grows with every change.
#[derive(Clone, Debug, Default)]
pub(crate) struct Snapshot {
    pub(crate) effective: Effective,
    pub(crate) revision: u64,
}

pub(crate) struct SettingsHub {
    file: Mutex<Option<SettingsFile>>,
    policy: Map<String, Value>,
    current: watch::Sender<Snapshot>,
    /// The daemon was started without the Docker CLI's default endpoint, whatever the settings say.
    default_endpoint_allowed: bool,
}

impl SettingsHub {
    /// Settings stored in `file`, or the defaults when there is none, under the administrator's `policy`.
    pub(crate) fn open(file: Option<PathBuf>, policy: Map<String, Value>, default_endpoint_allowed: bool) -> Self {
        let file = file.map(SettingsFile::new);
        let stored = match file.as_ref().map(SettingsFile::load).transpose() {
            Ok(stored) => stored.unwrap_or_default(),
            Err(error) => {
                tracing::warn!(error = %describe(&error), "the settings are unreadable; using the defaults");
                Settings::default()
            }
        };
        let effective = contr_settings::effective(&stored, &policy).unwrap_or_else(|error| {
            tracing::warn!(error = %describe(&error), "the settings are invalid; using the defaults");
            effective_defaults(&policy)
        });

        Self {
            file: Mutex::new(file),
            policy,
            current: watch::Sender::new(Snapshot { effective, revision: 1 }),
            default_endpoint_allowed,
        }
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        self.current.borrow().clone()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.current.subscribe()
    }

    /// What the backend applies itself from the current settings.
    pub(crate) fn backend_settings(&self) -> BackendSettings {
        let configured = &self.current.borrow().effective.configured;

        BackendSettings {
            serve_default_endpoint: self.default_endpoint_allowed && configured.engine.serve_default_pipe,
            engine: EngineSettings {
                proxy: match configured.proxy.mode {
                    ProxyMode::System => ProxyChoice::System,
                    ProxyMode::Manual => ProxyChoice::Manual(configured.proxy.manual()),
                    ProxyMode::None => ProxyChoice::Direct,
                },
                trust_system_certificates: configured.certificates.trust_system,
                registry_mirrors: configured.registry.mirrors.clone(),
            },
        }
    }

    /// Applies `change` to freshly read stored settings, saves them and publishes the result. Fails with ABORTED
    /// when `expected_revision` is set and stale; a failing `change` leaves the file as it was.
    pub(crate) async fn update<T>(
        &self,
        expected_revision: Option<u64>,
        backend: &dyn Backend,
        change: impl FnOnce(&mut Settings) -> Result<T, Status>,
    ) -> Result<(T, Snapshot), Status> {
        let file = self.file.lock().await;
        let file = file
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("this daemon has no settings file"))?;
        let revision = self.current.borrow().revision;
        if expected_revision.is_some_and(|expected| expected != revision) {
            return Err(Status::aborted("the settings changed since they were read"));
        }

        let mut stored = file.load().map_err(|error| unusable(&error))?;
        let result = change(&mut stored)?;
        let effective = contr_settings::effective(&stored, &self.policy)
            .map_err(|error| Status::invalid_argument(describe(&error)))?;
        file.save(&stored).map_err(|error| unusable(&error))?;

        let snapshot = Snapshot {
            effective,
            revision: revision + 1,
        };
        self.current.send_replace(snapshot.clone());
        backend.apply_settings(self.backend_settings());

        Ok((result, snapshot))
    }
}

/// Defaults under the policy, for stored settings that cannot be read.
fn effective_defaults(policy: &Map<String, Value>) -> Effective {
    contr_settings::effective(&Settings::default(), policy).unwrap_or_else(|error| {
        tracing::error!(error = %describe(&error), "the administrator's settings are invalid; ignoring them");
        Effective::default()
    })
}

/// The administrator's policy from `file` when given, else from the system's policy location; an unreadable policy
/// is logged and ignored.
pub(crate) fn load_policy(file: Option<&PathBuf>) -> Map<String, Value> {
    let text = match file {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        },
        None => contr_platform::policy::admin_settings(),
    };
    let parsed = text.map_err(|error| error.to_string()).and_then(|text| {
        text.map(|text| contr_settings::parse_policy(&text).map_err(|error| describe(&error)))
            .transpose()
    });

    match parsed {
        Ok(policy) => policy.unwrap_or_default(),
        Err(error) => {
            tracing::error!(%error, "cannot read the administrator's settings; ignoring them");
            Map::new()
        }
    }
}

pub(crate) fn unusable(error: &SettingsError) -> Status {
    Status::internal(describe(error))
}

fn describe(error: &SettingsError) -> String {
    match std::error::Error::source(error) {
        Some(cause) => format!("{error}: {cause}"),
        None => error.to_string(),
    }
}
