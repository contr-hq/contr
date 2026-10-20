use std::io;
use std::path::PathBuf;

use async_trait::async_trait;
use contr_backend::{
    Backend, BackendError, BackendKind, Blocker, Capability, EngineConnector, Probe, Progress, ProgressSink, ResetScope,
};
use contr_core::{ErrorReason, Remediation, paths, product};
use tokio::sync::Mutex;

use crate::images::GuestImages;
use crate::session::Session;
use crate::wsl;

/// Pipe the Docker CLI uses when neither a context nor `DOCKER_HOST` names another.
const DOCKER_DEFAULT_PIPE: &str = r"\\.\pipe\docker_engine";

/// Backend that runs the engine in two WSL 2 distributions: a runtime distribution replaced whenever its release
/// differs from the bundled image, and a data distribution that keeps engine data across those replacements.
pub struct WslBackend {
    images_dir: PathBuf,
    disks_dir: PathBuf,
    engine_pipe: String,
    shared_pipes: Vec<String>,
    session: Mutex<Option<Session>>,
}

impl WslBackend {
    /// Backend that installs the guest images described by the manifest in `images_dir`; with `default_pipe` it also
    /// serves the Docker CLI's default pipe whenever no other program owns it.
    pub fn new(images_dir: PathBuf, default_pipe: bool) -> io::Result<Self> {
        let disks_dir = paths::wsl_dir().ok_or_else(|| io::Error::other("LOCALAPPDATA is not set"))?;
        let engine_pipe = product::engine_pipe_name(&contr_transport::current_user_sid()?);
        let shared_pipes = if default_pipe {
            vec![DOCKER_DEFAULT_PIPE.to_owned()]
        } else {
            Vec::new()
        };

        Ok(Self {
            images_dir,
            disks_dir,
            engine_pipe,
            shared_pipes,
            session: Mutex::new(None),
        })
    }

    fn connector(&self) -> EngineConnector {
        EngineConnector::NamedPipe(self.engine_pipe.clone())
    }
}

#[async_trait]
impl Backend for WslBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Wsl
    }

    fn capabilities(&self) -> &'static [Capability] {
        &[]
    }

    async fn probe(&self) -> Result<Probe, BackendError> {
        let blockers = match wsl::version().await {
            Some(version) => {
                tracing::debug!(%version, "WSL found");
                Vec::new()
            }
            None => vec![Blocker {
                reason: ErrorReason::WslNotInstalled,
                remediations: vec![Remediation::INSTALL_WSL],
            }],
        };

        Ok(Probe { blockers })
    }

    async fn provision(&self, progress: &dyn ProgressSink) -> Result<(), BackendError> {
        let images = GuestImages::load(&self.images_dir)?;
        let installed = wsl::distributions().await?;
        let has = |name: &str| installed.iter().any(|installed| installed == name);

        if !has(product::WSL_DATA_DISTRO) {
            report(progress, "Installing the data distribution");
            wsl::import(
                product::WSL_DATA_DISTRO,
                &self.disks_dir.join(product::WSL_DATA_DISTRO),
                &images.data,
            )
            .await?;
        }

        if has(product::WSL_RUNTIME_DISTRO) {
            let release = wsl::release(product::WSL_RUNTIME_DISTRO).await?;
            if release == images.release {
                return Ok(());
            }
            report(
                progress,
                &format!("Updating the runtime distribution from {release} to {}", images.release),
            );
            wsl::unregister(product::WSL_RUNTIME_DISTRO).await?;
        } else {
            report(progress, "Installing the runtime distribution");
        }
        wsl::import(
            product::WSL_RUNTIME_DISTRO,
            &self.disks_dir.join(product::WSL_RUNTIME_DISTRO),
            &images.runtime,
        )
        .await?;

        Ok(())
    }

    async fn running(&self) -> Result<Option<EngineConnector>, BackendError> {
        let mut session = self.session.lock().await;
        let alive = session.as_mut().is_some_and(Session::is_alive);

        Ok(alive.then(|| self.connector()))
    }

    async fn start(&self, progress: &dyn ProgressSink) -> Result<EngineConnector, BackendError> {
        let mut slot = self.session.lock().await;
        if slot.as_mut().is_some_and(Session::is_alive) {
            return Ok(self.connector());
        }
        if let Some(stale) = slot.take() {
            stale.stop().await;
        }

        report(progress, "Starting the engine");
        let session = Session::start(
            product::WSL_RUNTIME_DISTRO,
            product::WSL_DATA_DISTRO,
            &self.engine_pipe,
            &self.shared_pipes,
        )
        .await?;
        *slot = Some(session);

        Ok(self.connector())
    }

    async fn stop(&self, progress: &dyn ProgressSink) -> Result<(), BackendError> {
        let Some(session) = self.session.lock().await.take() else {
            return Ok(());
        };
        report(progress, "Stopping the engine");
        session.stop().await;

        Ok(())
    }

    async fn reset(&self, scope: ResetScope, progress: &dyn ProgressSink) -> Result<(), BackendError> {
        let installed = wsl::distributions().await?;
        let mut targets = vec![product::WSL_RUNTIME_DISTRO];
        if scope == ResetScope::Factory {
            targets.push(product::WSL_DATA_DISTRO);
        }

        for name in targets {
            if installed.iter().any(|installed| installed == name) {
                report(progress, &format!("Removing {name}"));
                wsl::unregister(name).await?;
            }
        }
        Ok(())
    }
}

fn report(progress: &dyn ProgressSink, message: &str) {
    progress.report(Progress {
        message: message.to_owned(),
        fraction: None,
    });
}
