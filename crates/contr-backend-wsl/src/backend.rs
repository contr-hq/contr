use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use contr_backend::{
    Backend, BackendError, BackendKind, BackendSettings, Blocker, Capability, Check, CheckStatus, EngineConnector,
    EngineSettings, IntegrationState, Probe, Progress, ProgressSink, RemediationOutcome, ResetScope, WslDistribution,
    WslIntegration,
};
use contr_core::guest::{ConfigRejection, EngineConfig};
use contr_core::{ErrorReason, Remediation, paths, product};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::engine_config;
use crate::images::GuestImages;
use crate::integration::Integrations;
use crate::session::Session;
use crate::wsl;

/// Pipe the Docker CLI uses when neither a context nor `DOCKER_HOST` names another.
const DOCKER_DEFAULT_PIPE: &str = r"\\.\pipe\docker_engine";
/// Exit code of an installer whose changes take effect after a restart.
const RESTART_REQUIRED: u32 = 3010;

/// Backend that runs the engine in two WSL 2 distributions: a runtime distribution replaced whenever its release
/// differs from the bundled image, and a data distribution that keeps engine data across those replacements.
pub struct WslBackend {
    images_dir: PathBuf,
    disks_dir: PathBuf,
    engine_pipe: String,
    default_pipe: AtomicBool,
    engine: std::sync::Mutex<EngineSettings>,
    integrations: Integrations,
    session: Mutex<Option<Session>>,
}

impl WslBackend {
    /// Backend that installs the guest images described by the manifest in `images_dir`. The `integrated`
    /// distributions reach the engine while it runs, and the Docker CLI's default pipe is served once
    /// [`Backend::apply_settings`] asks for it.
    pub fn new(images_dir: PathBuf, integrated: BTreeSet<String>) -> io::Result<Self> {
        let disks_dir = paths::wsl_dir().ok_or_else(|| io::Error::other("LOCALAPPDATA is not set"))?;
        let engine_pipe = product::engine_pipe_name(&contr_transport::current_user_sid()?);

        Ok(Self {
            images_dir,
            disks_dir,
            engine_pipe,
            default_pipe: AtomicBool::new(false),
            engine: std::sync::Mutex::default(),
            integrations: Integrations::new(integrated),
            session: Mutex::new(None),
        })
    }

    fn connector(&self) -> EngineConnector {
        EngineConnector::NamedPipe(self.engine_pipe.clone())
    }

    /// Pipes besides the per-user one that the next session serves when no other program owns them.
    fn shared_pipes(&self) -> Vec<String> {
        if self.default_pipe.load(Ordering::Relaxed) {
            vec![DOCKER_DEFAULT_PIPE.to_owned()]
        } else {
            Vec::new()
        }
    }
}

#[async_trait]
impl Backend for WslBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Wsl
    }

    fn capabilities(&self) -> &'static [Capability] {
        &[
            Capability::WslIntegration,
            Capability::Reset,
            Capability::EngineConfiguration,
        ]
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

        report(progress, "Configuring the engine");
        let settings = self.engine.lock().map(|settings| settings.clone()).unwrap_or_default();
        let config = engine_config::resolve(&settings).await;
        report(progress, "Starting the engine");
        let session = Session::start(
            product::WSL_RUNTIME_DISTRO,
            product::WSL_DATA_DISTRO,
            &config,
            &self.engine_pipe,
            &self.shared_pipes(),
            &self.integrations,
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

    fn apply_settings(&self, settings: BackendSettings) {
        self.default_pipe
            .store(settings.serve_default_endpoint, Ordering::Relaxed);
        if let Ok(mut engine) = self.engine.lock() {
            *engine = settings.engine;
        }
    }

    async fn check_daemon_config(&self, daemon: &Map<String, Value>) -> Result<(), ConfigRejection> {
        let config = EngineConfig {
            daemon: daemon.clone(),
            certificates: String::new(),
        };
        match wsl::configure(product::WSL_RUNTIME_DISTRO, &config, wsl::Configure::Check).await {
            Err(wsl::ConfigureError::Rejected(rejection)) => Err(rejection),
            Err(wsl::ConfigureError::Io(error)) => {
                tracing::info!(%error, "cannot ask the engine about daemon.json; its next start will");
                Ok(())
            }
            Ok(()) => Ok(()),
        }
    }

    fn remediations(&self) -> &'static [Remediation] {
        &[Remediation::INSTALL_WSL]
    }

    async fn diagnose(&self) -> Vec<Check> {
        let images = GuestImages::load(&self.images_dir);
        let mut checks = vec![match &images {
            Ok(images) => Check::new(
                "guest-images",
                "Guest images",
                CheckStatus::Pass,
                format!("Release {} is bundled.", images.release),
            ),
            Err(error) => Check::new(
                "guest-images",
                "Guest images",
                CheckStatus::Fail,
                format!("The bundled guest images are unusable: {error}"),
            ),
        }];
        if wsl::version().await.is_none() {
            checks.push(Check::new(
                "runtime-distro",
                "Runtime distribution",
                CheckStatus::Skip,
                "WSL is not available.",
            ));
            checks.push(Check::new(
                "data-distro",
                "Data distribution",
                CheckStatus::Skip,
                "WSL is not available.",
            ));
            return checks;
        }

        let installed = wsl::distributions().await.unwrap_or_default();
        let bundled = images.as_ref().ok().map(|images| images.release.as_str());
        checks.push(runtime_check(&installed, bundled).await);
        checks.push(if installed.iter().any(|name| name == product::WSL_DATA_DISTRO) {
            Check::new(
                "data-distro",
                "Data distribution",
                CheckStatus::Pass,
                "Keeps engine data across runtime updates.",
            )
        } else {
            Check::new(
                "data-distro",
                "Data distribution",
                CheckStatus::Skip,
                "Created at the first start.",
            )
        });
        checks.extend(self.default_pipe_check().await);
        checks.push(integration_check(&self.integrations.states()));

        checks
    }

    async fn remediate(
        &self,
        remediation: &Remediation,
        progress: &dyn ProgressSink,
    ) -> Result<RemediationOutcome, BackendError> {
        if *remediation != Remediation::INSTALL_WSL {
            return Err(io::Error::other(format!("cannot run {}", remediation.action_id)).into());
        }

        report(progress, "Installing WSL; approve the administrator prompt");
        let code = tokio::task::spawn_blocking(|| {
            contr_platform::elevation::run_elevated("wsl.exe", "--install --no-distribution")
        })
        .await
        .map_err(io::Error::other)??;
        if code != 0 && code != RESTART_REQUIRED {
            return Err(io::Error::other(format!("wsl --install exited with {code}")).into());
        }

        Ok(RemediationOutcome {
            restart_required: code == RESTART_REQUIRED || wsl::version().await.is_none(),
            logout_required: false,
        })
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

#[async_trait]
impl WslIntegration for WslBackend {
    async fn distributions(&self) -> Result<Vec<WslDistribution>, BackendError> {
        let own = [product::WSL_RUNTIME_DISTRO, product::WSL_DATA_DISTRO];

        Ok(wsl::list()
            .await?
            .into_iter()
            .filter(|listed| !own.contains(&listed.name.as_str()))
            .map(|listed| WslDistribution {
                integration: self.integrations.state(&listed.name),
                name: listed.name,
                version: listed.version,
                is_default: listed.is_default,
            })
            .collect())
    }

    async fn set_integrated(&self, name: &str, integrated: bool) {
        self.integrations.set(name, integrated);
    }
}

impl WslBackend {
    async fn default_pipe_check(&self) -> Option<Check> {
        const ID: &str = "default-pipe";
        const TITLE: &str = "Standard Docker pipe";

        if !self.default_pipe.load(Ordering::Relaxed) {
            return None;
        }
        let mut session = self.session.lock().await;
        let alive = session.as_mut().is_some_and(Session::is_alive);
        let serving = session
            .as_ref()
            .filter(|_| alive)
            .map(|session| session.serves(DOCKER_DEFAULT_PIPE));
        let check = match serving {
            Some(true) => Check::new(
                ID,
                TITLE,
                CheckStatus::Pass,
                format!("Contr serves {DOCKER_DEFAULT_PIPE}, so tools that ignore Docker contexts reach this engine."),
            ),
            Some(false) => Check::new(
                ID,
                TITLE,
                CheckStatus::Warn,
                format!(
                    "Another program serves {DOCKER_DEFAULT_PIPE}, so tools that ignore Docker contexts reach it \
                     instead of this engine."
                ),
            ),
            None => Check::new(ID, TITLE, CheckStatus::Skip, "The engine is not running."),
        };

        Some(check)
    }
}

async fn runtime_check(installed: &[String], bundled: Option<&str>) -> Check {
    const ID: &str = "runtime-distro";
    const TITLE: &str = "Runtime distribution";

    if !installed.iter().any(|name| name == product::WSL_RUNTIME_DISTRO) {
        return Check::new(ID, TITLE, CheckStatus::Skip, "Created at the first start.");
    }
    match wsl::release(product::WSL_RUNTIME_DISTRO).await {
        Ok(release) => {
            let detail = match bundled {
                Some(bundled) if bundled != release => {
                    format!("Release {release}; release {bundled} replaces it at the next start.")
                }
                _ => format!("Release {release}."),
            };
            Check::new(ID, TITLE, CheckStatus::Pass, detail)
        }
        Err(error) => Check::new(
            ID,
            TITLE,
            CheckStatus::Warn,
            format!("Cannot read its release: {error}"),
        ),
    }
}

fn integration_check(states: &[(String, IntegrationState)]) -> Check {
    const ID: &str = "wsl-integrations";
    const TITLE: &str = "WSL distributions";

    let failures: Vec<String> = states
        .iter()
        .filter_map(|(name, state)| match state {
            IntegrationState::Failed(reason) => Some(format!("{name}: {reason}")),
            _ => None,
        })
        .collect();
    if states.is_empty() {
        Check::new(ID, TITLE, CheckStatus::Skip, "No WSL distribution is integrated.")
    } else if !failures.is_empty() {
        Check::new(ID, TITLE, CheckStatus::Warn, failures.join("; "))
    } else if states.iter().all(|(_, state)| *state == IntegrationState::Active) {
        let verb = if states.len() == 1 { "reaches" } else { "reach" };
        Check::new(
            ID,
            TITLE,
            CheckStatus::Pass,
            format!("{} {verb} the engine.", names(states)),
        )
    } else {
        Check::new(ID, TITLE, CheckStatus::Skip, "Integrations start with the engine.")
    }
}

fn names(states: &[(String, IntegrationState)]) -> String {
    states
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn report(progress: &dyn ProgressSink, message: &str) {
    progress.report(Progress {
        message: message.to_owned(),
        fraction: None,
    });
}
