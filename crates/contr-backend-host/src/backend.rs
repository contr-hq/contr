use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use contr_backend::{
    Backend, BackendError, BackendKind, Blocker, Capability, Check, CheckStatus, EngineConnector, Probe, Progress,
    ProgressSink, RemediationOutcome, ResetScope,
};
use contr_core::{ErrorReason, Remediation};
use tokio::net::UnixStream;
use tokio::time::{self, Instant};

use crate::discovery::{Candidate, Flavor, candidates};
use crate::remediate;
use crate::systemd::Systemd;

const SOCKET_WAIT: Duration = Duration::from_secs(30);
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Backend for an engine the host's package manager installed; Contr only starts and stops its systemd units.
pub struct HostBackend {
    explicit_socket: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
}

impl HostBackend {
    /// Backend that discovers the engine, or uses `explicit_socket` without managing its lifecycle.
    pub fn new(explicit_socket: Option<PathBuf>) -> Self {
        Self {
            explicit_socket,
            runtime_dir: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        }
    }

    fn candidates(&self) -> Vec<Candidate> {
        candidates(self.runtime_dir.as_deref(), self.explicit_socket.as_deref())
    }

    /// First candidate whose socket accepts a connection.
    async fn reachable(&self) -> Result<Option<Candidate>, BackendError> {
        let mut denied = false;

        for candidate in self.candidates() {
            match UnixStream::connect(&candidate.socket).await {
                Ok(_) => return Ok(Some(candidate)),
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => denied = true,
                Err(_) => {}
            }
        }

        if denied {
            return Err(BackendError::Blocked(Blocker {
                reason: ErrorReason::SocketPermissionDenied,
                remediations: vec![Remediation::SETUP_ROOTLESS, Remediation::JOIN_DOCKER_GROUP],
            }));
        }
        Ok(None)
    }

    /// First candidate whose start unit is installed.
    async fn startable(&self) -> Option<Candidate> {
        for candidate in self.candidates() {
            let Some(unit) = candidate.start_unit else {
                continue;
            };
            match Systemd::connect(candidate.bus).await {
                Ok(systemd) => match systemd.is_installed(unit).await {
                    Ok(true) => return Some(candidate),
                    Ok(false) => {}
                    Err(error) => tracing::debug!(%error, unit, "cannot query systemd"),
                },
                Err(error) => tracing::debug!(%error, bus = ?candidate.bus, "cannot reach systemd"),
            }
        }

        None
    }

    fn missing_engine(&self) -> Blocker {
        if self.explicit_socket.is_some() {
            Blocker::new(ErrorReason::EngineNotRunning)
        } else {
            Blocker {
                reason: ErrorReason::EngineNotInstalled,
                remediations: vec![Remediation::INSTALL_ENGINE],
            }
        }
    }
}

#[async_trait]
impl Backend for HostBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Host
    }

    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::Compose, Capability::Kubernetes]
    }

    async fn probe(&self) -> Result<Probe, BackendError> {
        let blockers = match self.reachable().await {
            Ok(Some(_)) => Vec::new(),
            Ok(None) if self.startable().await.is_some() => Vec::new(),
            Ok(None) => vec![self.missing_engine()],
            Err(BackendError::Blocked(blocker)) => vec![blocker],
            Err(error) => return Err(error),
        };

        Ok(Probe { blockers })
    }

    async fn provision(&self, _progress: &dyn ProgressSink) -> Result<(), BackendError> {
        Ok(())
    }

    async fn running(&self) -> Result<Option<EngineConnector>, BackendError> {
        Ok(self.reachable().await?.map(|candidate| connector(&candidate)))
    }

    async fn start(&self, progress: &dyn ProgressSink) -> Result<EngineConnector, BackendError> {
        if let Some(candidate) = self.reachable().await? {
            return Ok(connector(&candidate));
        }

        let candidate = self
            .startable()
            .await
            .ok_or_else(|| BackendError::Blocked(self.missing_engine()))?;
        let unit = candidate.start_unit.unwrap_or_default();

        report(progress, format!("Starting {unit}"));
        start_unit(&candidate, unit)
            .await
            .map_err(|error| io::Error::other(format!("systemd did not start {unit}: {error}")))?;

        report(progress, format!("Waiting for {}", candidate.socket.display()));
        wait_for_socket(&candidate.socket, unit).await?;

        Ok(connector(&candidate))
    }

    async fn stop(&self, progress: &dyn ProgressSink) -> Result<(), BackendError> {
        let Some(candidate) = self.reachable().await? else {
            return Ok(());
        };
        if candidate.stop_units.is_empty() {
            return Err(BackendError::Blocked(Blocker::new(ErrorReason::FeatureUnavailable)));
        }

        let systemd = Systemd::connect(candidate.bus).await.map_err(io::Error::other)?;
        for &unit in candidate.stop_units {
            if !systemd.is_installed(unit).await.unwrap_or(false) {
                continue;
            }
            report(progress, format!("Stopping {unit}"));
            systemd
                .stop(unit)
                .await
                .map_err(|error| io::Error::other(format!("systemd did not stop {unit}: {error}")))?;
        }

        Ok(())
    }

    async fn diagnose(&self) -> Vec<Check> {
        const ID: &str = "host-engine";
        const TITLE: &str = "Host engine";

        let check = match self.reachable().await {
            Ok(Some(candidate)) => Check::new(
                ID,
                TITLE,
                CheckStatus::Pass,
                format!(
                    "{} answers at {}.",
                    flavor_name(candidate.flavor),
                    candidate.socket.display()
                ),
            ),
            Ok(None) => match self.startable().await {
                Some(candidate) => Check::new(
                    ID,
                    TITLE,
                    CheckStatus::Pass,
                    format!("{} is installed and starts with Contr.", flavor_name(candidate.flavor)),
                ),
                None => Check::new(ID, TITLE, CheckStatus::Skip, "No engine is installed."),
            },
            Err(_) => Check::new(ID, TITLE, CheckStatus::Skip, "No engine socket is usable by this user."),
        };

        vec![check]
    }

    async fn reset(&self, _scope: ResetScope, _progress: &dyn ProgressSink) -> Result<(), BackendError> {
        Err(BackendError::Blocked(Blocker::new(ErrorReason::FeatureUnavailable)))
    }

    fn remediations(&self) -> &'static [Remediation] {
        &[
            Remediation::INSTALL_ENGINE,
            Remediation::JOIN_DOCKER_GROUP,
            Remediation::SETUP_ROOTLESS,
        ]
    }

    async fn remediate(
        &self,
        remediation: &Remediation,
        progress: &dyn ProgressSink,
    ) -> Result<RemediationOutcome, BackendError> {
        match *remediation {
            Remediation::INSTALL_ENGINE => remediate::install_engine(progress).await,
            Remediation::JOIN_DOCKER_GROUP => remediate::join_docker_group(progress).await,
            Remediation::SETUP_ROOTLESS => remediate::setup_rootless(progress).await,
            _ => Err(io::Error::other(format!("cannot run {}", remediation.action_id)).into()),
        }
    }
}

async fn start_unit(candidate: &Candidate, unit: &str) -> zbus::Result<()> {
    Systemd::connect(candidate.bus).await?.start(unit).await
}

fn flavor_name(flavor: Flavor) -> &'static str {
    match flavor {
        Flavor::DockerRootless => "Docker (rootless)",
        Flavor::DockerRootful => "Docker",
        Flavor::PodmanRootless => "Podman (rootless)",
        Flavor::PodmanRootful => "Podman",
        Flavor::External => "The configured engine",
    }
}

fn connector(candidate: &Candidate) -> EngineConnector {
    EngineConnector::UnixSocket(candidate.socket.clone())
}

fn report(progress: &dyn ProgressSink, message: String) {
    progress.report(Progress {
        message,
        fraction: None,
    });
}

async fn wait_for_socket(socket: &Path, unit: &str) -> io::Result<()> {
    let deadline = Instant::now() + SOCKET_WAIT;

    while UnixStream::connect(socket).await.is_err() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{unit} did not open {} within {} seconds",
                    socket.display(),
                    SOCKET_WAIT.as_secs()
                ),
            ));
        }
        time::sleep(SOCKET_POLL_INTERVAL).await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Discard;

    impl ProgressSink for Discard {
        fn report(&self, _: Progress) {}
    }

    #[tokio::test]
    async fn reports_an_explicit_socket_that_listens() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("engine.sock");
        let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let backend = HostBackend::new(Some(socket.clone()));

        assert_eq!(
            backend.running().await.unwrap(),
            Some(EngineConnector::UnixSocket(socket))
        );
        assert!(backend.probe().await.unwrap().is_ready());
    }

    #[tokio::test]
    async fn an_explicit_socket_without_engine_is_not_running_and_cannot_be_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let backend = HostBackend::new(Some(dir.path().join("missing.sock")));

        assert_eq!(backend.running().await.unwrap(), None);
        let probe = backend.probe().await.unwrap();
        assert_eq!(probe.blockers, [Blocker::new(ErrorReason::EngineNotRunning)]);
        assert!(backend.stop(&Discard).await.is_ok());
    }
}
