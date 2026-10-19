//! Engine backend contract: each platform runtime implements [`Backend`], and the daemon drives every one of them
//! through the same lifecycle.

#![warn(missing_docs)]

use std::io;
use std::path::PathBuf;

use async_trait::async_trait;
use contr_core::{ErrorReason, Remediation};

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

    /// Starts the engine and returns where its Docker API listens.
    async fn start(&self, progress: &dyn ProgressSink) -> Result<EngineConnector, BackendError>;

    /// Stops the engine and every running container.
    async fn stop(&self, progress: &dyn ProgressSink) -> Result<(), BackendError>;

    /// Restores the runtime to a clean state; the engine must be stopped first.
    async fn reset(&self, scope: ResetScope, progress: &dyn ProgressSink) -> Result<(), BackendError>;
}
