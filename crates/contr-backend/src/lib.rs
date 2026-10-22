//! Engine backend contract: each platform runtime implements [`Backend`], and the daemon drives every one of them
//! through the same lifecycle.

#![warn(missing_docs)]

use std::collections::BTreeSet;
use std::io;
use std::net::IpAddr;
use std::path::PathBuf;

use async_trait::async_trait;
use contr_core::guest::ConfigRejection;
use contr_core::proxy::Proxy;
use contr_core::{ErrorReason, Remediation};
use serde_json::{Map, Value};

#[cfg(feature = "fake")]
mod fake;
mod supervisor;

#[cfg(feature = "fake")]
pub use fake::FakeBackend;
pub use supervisor::{EngineHealth, EngineStatus, EngineSupervisor, OperationError, OperationEvent, SupervisorConfig};

/// Platform runtime that hosts the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// Docker Engine installed on the Linux host.
    Host,
    /// Contr's own WSL 2 distributions.
    Wsl,
    /// QEMU/KVM virtual machine on Linux.
    Qemu,
    /// In-memory engine for tests.
    Fake,
}

/// Optional feature a backend may support; clients hide features their backend lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Compose projects.
    Compose,
    /// The single-node Kubernetes cluster.
    Kubernetes,
    /// CPU, memory and disk limits for the engine.
    ResourceLimits,
    /// Engine access from the user's own WSL distributions.
    WslIntegration,
    /// Published container ports forwarded by Contr instead of the platform.
    HostPortForwarding,
    /// Restoring the engine to a clean state, which only a backend that owns its engine offers.
    Reset,
    /// Applying [`EngineSettings`], which only a backend that owns its engine does.
    EngineConfiguration,
}

/// Engine lifecycle state reported to clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineState {
    /// Nothing is provisioned yet.
    Uninitialized,
    /// First-time setup is running.
    Provisioning,
    /// Provisioned and not running.
    Stopped,
    /// Starting; the Docker API is not reachable yet.
    Starting,
    /// The Docker API answers.
    Running,
    /// Stopping; containers are being stopped.
    Stopping,
    /// The runtime is being replaced; engine data is kept.
    Updating,
    /// A reset is in progress.
    Resetting,
    /// The last transition failed.
    Failed(ErrorReason),
}

/// Where the daemon reaches the running engine's Docker API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineConnector {
    /// Unix domain socket.
    UnixSocket(PathBuf),
    /// Windows named pipe.
    NamedPipe(String),
}

/// What a reset removes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetScope {
    /// Recreates the runtime and keeps images, containers and volumes.
    Runtime,
    /// Deletes all engine data and returns to [`EngineState::Uninitialized`].
    Factory,
}

/// Unmet prerequisite, with the actions that resolve it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocker {
    /// Why the backend cannot proceed.
    pub reason: ErrorReason,
    /// Actions that resolve the blocker, in the order clients should offer them.
    pub remediations: Vec<Remediation>,
}

impl Blocker {
    /// Blocker that no action of the daemon resolves.
    pub fn new(reason: ErrorReason) -> Self {
        Self {
            reason,
            remediations: Vec::new(),
        }
    }
}

/// Result of checking a backend's prerequisites.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    /// Unmet prerequisites; empty when the backend can start.
    pub blockers: Vec<Blocker>,
}

impl Probe {
    /// The backend can be provisioned and started.
    pub fn is_ready(&self) -> bool {
        self.blockers.is_empty()
    }
}

/// Step of a long-running backend operation.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    /// User-facing description of the current step.
    pub message: String,
    /// Completed share in `0.0..=1.0`, or `None` while indeterminate.
    pub fraction: Option<f32>,
}

/// Receives progress of long-running operations and forwards it to the client stream.
pub trait ProgressSink: Send + Sync {
    /// Reports one step; must not block.
    fn report(&self, progress: Progress);
}

/// Failure of a backend operation.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// A prerequisite is unmet.
    #[error("backend is blocked: {}", .0.reason)]
    Blocked(Blocker),
    /// An operating-system call failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The user dismissed a prompt the operation needs, so nothing changed.
    #[error("the prompt was dismissed")]
    Dismissed,
}

/// Platform runtime the daemon provisions, starts and stops.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Runtime kind; constant for the lifetime of the value.
    fn kind(&self) -> BackendKind;

    /// Features this backend supports.
    fn capabilities(&self) -> &'static [Capability];

    /// Checks prerequisites without changing the system.
    async fn probe(&self) -> Result<Probe, BackendError>;

    /// Performs first-time setup; calling it again on a provisioned backend does nothing.
    async fn provision(&self, progress: &dyn ProgressSink) -> Result<(), BackendError>;

    /// Returns where the engine's Docker API listens when the engine already runs; never starts it.
    async fn running(&self) -> Result<Option<EngineConnector>, BackendError>;

    /// Starts the engine and returns where its Docker API listens; starting a running engine returns at once.
    async fn start(&self, progress: &dyn ProgressSink) -> Result<EngineConnector, BackendError>;

    /// Stops the engine and every running container.
    async fn stop(&self, progress: &dyn ProgressSink) -> Result<(), BackendError>;

    /// Restores the runtime to a clean state; the engine must be stopped first.
    async fn reset(&self, scope: ResetScope, progress: &dyn ProgressSink) -> Result<(), BackendError>;

    /// Takes the settings the backend applies itself; the daemon calls it before the first start and after every
    /// change. A change applies at the next engine start.
    fn apply_settings(&self, settings: BackendSettings) {
        let _ = settings;
    }

    /// Takes the TCP ports the engine publishes now; the daemon calls it after every change, and with none while the
    /// engine is not running. A backend whose engine runs on another network stack serves them on this machine.
    fn publish_ports(&self, ports: &BTreeSet<PublishedPort>) {
        let _ = ports;
    }

    /// Asks the engine whether it takes `daemon` as the user's `daemon.json`, and fails with its objections when it
    /// does not. A backend that cannot ask accepts it, and the engine's next start reports the problem instead.
    async fn check_daemon_config(&self, daemon: &Map<String, Value>) -> Result<(), ConfigRejection> {
        let _ = daemon;
        Ok(())
    }

    /// Remediations [`Backend::remediate`] runs; every remediation a blocker or check offers is one of them.
    fn remediations(&self) -> &'static [Remediation] {
        &[]
    }

    /// Checks of the backend's own state beyond [`Backend::probe`], in the order clients show them.
    async fn diagnose(&self) -> Vec<Check> {
        Vec::new()
    }

    /// The backend's state and recent logs for a diagnostics bundle; the daemon redacts them before they leave it.
    async fn diagnostic_files(&self) -> Vec<DiagnosticFile> {
        Vec::new()
    }

    /// Runs `remediation`, one of [`Backend::remediations`].
    async fn remediate(
        &self,
        remediation: &Remediation,
        progress: &dyn ProgressSink,
    ) -> Result<RemediationOutcome, BackendError> {
        let _ = progress;
        Err(BackendError::Io(io::Error::other(format!(
            "this backend cannot run {}",
            remediation.action_id
        ))))
    }
}

/// TCP port the engine publishes for a container.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublishedPort {
    /// Address the engine binds; unspecified for every interface.
    pub address: IpAddr,
    /// Port number; never zero.
    pub port: u16,
}

/// User settings a backend applies itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackendSettings {
    /// Also serve the engine where the Docker CLI looks when no context names another endpoint, while no other
    /// program does.
    pub serve_default_endpoint: bool,
    /// Settings of an engine the backend owns.
    pub engine: EngineSettings,
}

/// How an engine the backend owns reaches registries; see [`Capability::EngineConfiguration`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineSettings {
    /// Proxy for the engine's own connections.
    pub proxy: ProxyChoice,
    /// New containers and builds also receive that proxy in their environment.
    pub pass_proxy_to_containers: bool,
    /// The engine also trusts the root certificates the operating system trusts.
    pub trust_system_certificates: bool,
    /// Docker Hub mirrors the engine tries before Docker Hub.
    pub registry_mirrors: Vec<String>,
    /// The user's `daemon.json`, which holds none of the keys the backend sets.
    pub daemon: Map<String, Value>,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            proxy: ProxyChoice::System,
            pass_proxy_to_containers: false,
            trust_system_certificates: true,
            registry_mirrors: Vec::new(),
            daemon: Map::new(),
        }
    }
}

/// Where the engine's proxy comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyChoice {
    /// The operating system's proxy, looked up when the engine starts.
    System,
    /// These servers.
    Manual(Proxy),
    /// Direct connections.
    Direct,
}

/// Text file of a diagnostics bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticFile {
    /// File name inside the backend's directory of the bundle.
    pub name: String,
    /// Contents, unredacted.
    pub text: String,
}

/// Result of one diagnostic check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// Stable identifier clients may key translations on.
    pub id: &'static str,
    /// User-facing name of what is checked.
    pub title: &'static str,
    /// Outcome.
    pub status: CheckStatus,
    /// What the check found, in one sentence.
    pub detail: String,
    /// Fixes the backend can run, in the order clients should offer them.
    pub remediations: Vec<Remediation>,
}

impl Check {
    /// Check with no remediation.
    pub fn new(id: &'static str, title: &'static str, status: CheckStatus, detail: impl Into<String>) -> Self {
        Self {
            id,
            title,
            status,
            detail: detail.into(),
            remediations: Vec::new(),
        }
    }
}

/// Outcome of a diagnostic check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckStatus {
    /// Works.
    Pass,
    /// Works, with a limitation the user may want to remove.
    Warn,
    /// Keeps the engine or a feature from working.
    Fail,
    /// Not checked, because it does not apply now or an earlier check failed.
    Skip,
}

/// What the user must still do after a remediation succeeded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemediationOutcome {
    /// The computer must restart before the fix takes effect.
    pub restart_required: bool,
    /// The user must sign out and back in before the fix takes effect.
    pub logout_required: bool,
}

/// One of the user's own WSL distributions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WslDistribution {
    /// Name `wsl.exe` lists it under.
    pub name: String,
    /// WSL version that runs it; only version 2 distributions can reach the engine.
    pub version: u8,
    /// `wsl.exe` starts this distribution when none is named.
    pub is_default: bool,
    /// Its access to the engine.
    pub integration: IntegrationState,
}

/// Engine access from one WSL distribution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrationState {
    /// Not requested.
    Disabled,
    /// Requested; waits for the engine to run or for the distribution to start serving it.
    Pending,
    /// `docker` in the distribution reaches the engine.
    Active,
    /// Requested, but the last attempt failed for the given reason; it is retried.
    Failed(String),
}

/// Engine access from the user's own WSL distributions, while the engine runs.
#[async_trait]
pub trait WslIntegration: Send + Sync + 'static {
    /// The user's distributions, without the backend's own.
    async fn distributions(&self) -> Result<Vec<WslDistribution>, BackendError>;

    /// Requests or withdraws engine access from distribution `name`; withdrawing removes what the integration
    /// added to the distribution.
    async fn set_integrated(&self, name: &str, integrated: bool);
}
