//! The Contr daemon: composes a backend, its supervisor, the engine cache and the gRPC services, and serves them on
//! the per-user local endpoint.

mod kubernetes;
mod operations;
mod ports;
mod services;
mod settings;
mod slot;
mod status;
mod updates;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use contr_api::compose::v1::compose_service_server::ComposeServiceServer;
use contr_api::containers::v1::containers_service_server::ContainersServiceServer;
use contr_api::daemon::v1::daemon_service_server::DaemonServiceServer;
use contr_api::diagnostics::v1::diagnostics_service_server::DiagnosticsServiceServer;
use contr_api::engine::v1::engine_service_server::EngineServiceServer;
use contr_api::images::v1::images_service_server::ImagesServiceServer;
use contr_api::integration::v1::integration_service_server::IntegrationServiceServer;
use contr_api::kubernetes::v1::kubernetes_service_server::KubernetesServiceServer;
use contr_api::networks::v1::networks_service_server::NetworksServiceServer;
use contr_api::operations::v1::operations_service_server::OperationsServiceServer;
use contr_api::settings::v1::settings_service_server::SettingsServiceServer;
use contr_api::updates::v1::updates_service_server::UpdatesServiceServer;
use contr_api::volumes::v1::volumes_service_server::VolumesServiceServer;
use contr_backend::{Backend, BackendKind, EngineConnector, EngineHealth, EngineSupervisor, SupervisorConfig};
use contr_core::{paths, product};
use contr_diagnostics::LogRing;
use contr_transport::{DiscoveryFile, Endpoint, Listener, TransportError};
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tonic::transport::server::Router;

use crate::kubernetes::KubernetesHub;
use crate::operations::Operations;
use crate::services::{
    CliLocations, ComposeApi, ContainersApi, DaemonApi, DiagnosticsApi, EngineApi, ImagesApi, IntegrationApi,
    KubernetesApi, NetworksApi, OperationsApi, SettingsApi, Shared, UpdatesApi, VolumesApi, WslAccess,
};
use crate::settings::SettingsHub;
use crate::slot::{DockerContext, EngineSlot};
use crate::updates::UpdatesHub;

/// Backend the daemon runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendChoice {
    /// The engine installed on the Linux host.
    Host,
    /// Contr's own WSL 2 distributions.
    #[cfg(windows)]
    Wsl,
    /// A fake engine serving fixtures.
    #[cfg(feature = "fake")]
    Fake,
}

/// Daemon settings from the command line.
#[derive(Clone, Debug)]
pub struct Config {
    pub backend: BackendChoice,
    /// Endpoint to serve instead of the activated or per-user default one; no discovery file is written for it.
    pub listen: Option<Endpoint>,
    /// Engine socket for the host backend instead of discovery.
    pub engine_socket: Option<PathBuf>,
    /// Fixture file for the fake backend instead of the built-in set.
    pub fixtures: Option<PathBuf>,
    /// Directory holding the guest images and their manifest for the WSL backend.
    pub guest_dir: Option<PathBuf>,
    /// Also serves the engine on the Docker CLI's default pipe when no other program owns it.
    pub default_pipe: bool,
    /// Directory holding the Docker CLI tools the daemon installs for the user.
    pub cli_dir: Option<PathBuf>,
    /// Keeps the Docker CLI context pointed at the running engine.
    pub docker_context: bool,
    /// The user's settings file instead of the per-user default.
    pub settings_file: Option<PathBuf>,
    /// File holding the administrator's settings instead of the system's policy location.
    pub admin_settings: Option<PathBuf>,
    /// Compose CLI plugin instead of the bundled one or the one the Docker CLI would run.
    pub compose: Option<PathBuf>,
    /// Kubeconfig file that receives the cluster's context instead of the one kubectl writes to.
    pub kubeconfig: Option<PathBuf>,
    /// The daemon's recent log, which diagnostics bundles carry.
    pub log: Option<LogRing>,
    /// GitHub releases endpoint to check for updates instead of the core's own.
    pub update_feed: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("cannot serve the API")]
    Serve(#[from] tonic::transport::Error),
    #[error("the {0} backend is not available on this platform")]
    UnsupportedBackend(&'static str),
    #[error("cannot set up the {backend} backend")]
    Backend {
        backend: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot load fixtures from {}", path.display())]
    Fixtures {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// How long calls still open when the daemon begins to stop may run before it stops serving them.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Serves the API until `shutdown` completes. Endless streams end at once; other calls, such as a pull, get
/// [`SHUTDOWN_GRACE`] to finish.
pub async fn run(config: Config, shutdown: impl Future<Output = ()> + Send) -> Result<(), DaemonError> {
    let listener = bind(config.listen.as_ref())?;
    let settings_file = config.settings_file.clone().or_else(paths::settings_file);
    let policy = settings::load_policy(config.admin_settings.as_ref());
    let settings = Arc::new(SettingsHub::open(settings_file.clone(), policy, config.default_pipe));
    let (backend, wsl) = backend(&config, settings_file.as_deref())?;
    backend.apply_settings(settings.backend_settings());
    // Only the WSL backend owns its engine; a host engine its administrator stopped must stay stopped.
    let supervision = SupervisorConfig {
        recover: backend.kind() == BackendKind::Wsl,
        ..SupervisorConfig::default()
    };
    let supervisor = EngineSupervisor::spawn(Arc::clone(&backend), Arc::new(Health), supervision).await;
    if settings.snapshot().effective.configured.engine.start_with_daemon {
        start_engine(supervisor.clone());
    }
    let context = docker_context(&config, backend.kind());
    let slot = EngineSlot::spawn(&supervisor, context.clone());
    ports::spawn(&slot, Arc::clone(&backend));
    let kubernetes = KubernetesHub::spawn(
        &slot,
        &settings,
        config.kubeconfig.clone().or_else(contr_kube::kubeconfig::default_path),
    );
    let updates = UpdatesHub::spawn(
        Arc::clone(&settings),
        config
            .update_feed
            .clone()
            .unwrap_or_else(|| product::RELEASES_API.to_owned()),
    );
    let stopping = CancellationToken::new();
    let shared = Arc::new(Shared {
        supervisor,
        slot,
        backend_kind: backend.kind(),
        capabilities: backend.capabilities(),
        started_at: SystemTime::now(),
        cli: cli_locations(config.cli_dir.clone()),
        wsl,
        backend: Arc::clone(&backend),
        settings,
        docker_context: context,
        kubernetes,
        updates,
        log: config.log.clone(),
        operations: Operations::new(stopping.clone()),
        stopping: stopping.clone(),
    });

    let endpoint = listener.endpoint();
    let discovery = config
        .listen
        .is_none()
        .then(paths::discovery_file)
        .flatten()
        .zip(endpoint.as_ref())
        .and_then(|(path, endpoint)| write_discovery(path, endpoint));
    notify_ready();
    tracing::info!(
        endpoint = endpoint.as_ref().map(ToString::to_string),
        backend = ?shared.backend_kind,
        "serving"
    );

    let served = router(shared, config.compose.clone()).serve_with_incoming_shutdown(listener.incoming(), {
        let stopping = stopping.clone();
        async move {
            shutdown.await;
            stopping.cancel();
        }
    });
    let served = tokio::select! {
        served = served => served,
        () = async {
            stopping.cancelled().await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => {
            tracing::warn!(seconds = SHUTDOWN_GRACE.as_secs(), "calls still open after the grace period were cut off");
            Ok(())
        }
    };

    if let Some(path) = discovery {
        DiscoveryFile::remove(&path, std::process::id());
    }
    Ok(served?)
}

/// Starts the engine in the background, as the settings ask when the daemon starts.
fn start_engine(supervisor: EngineSupervisor) {
    tokio::spawn(async move {
        let mut events = supervisor.start().await;
        while let Some(event) = futures_util::StreamExt::next(&mut events).await {
            if let contr_backend::OperationEvent::Finished(Err(error)) = event {
                tracing::warn!(%error, "the engine did not start with the daemon");
            }
        }
    });
}

/// Every service of the API over `shared`; `compose` is the Compose program given on the command line.
fn router(shared: Arc<Shared>, compose: Option<PathBuf>) -> Router {
    let compose = ComposeApi::new(Arc::clone(&shared), compose);
    Server::builder()
        .add_service(DaemonServiceServer::new(DaemonApi(Arc::clone(&shared))))
        .add_service(EngineServiceServer::new(EngineApi(Arc::clone(&shared))))
        .add_service(ContainersServiceServer::new(ContainersApi(Arc::clone(&shared))))
        .add_service(ImagesServiceServer::new(ImagesApi(Arc::clone(&shared))))
        .add_service(IntegrationServiceServer::new(IntegrationApi(Arc::clone(&shared))))
        .add_service(DiagnosticsServiceServer::new(DiagnosticsApi(Arc::clone(&shared))))
        .add_service(SettingsServiceServer::new(SettingsApi(Arc::clone(&shared))))
        .add_service(VolumesServiceServer::new(VolumesApi(Arc::clone(&shared))))
        .add_service(ComposeServiceServer::new(compose.clone()))
        .add_service(OperationsServiceServer::new(OperationsApi::new(
            Arc::clone(&shared),
            compose,
        )))
        .add_service(KubernetesServiceServer::new(KubernetesApi(Arc::clone(&shared))))
        .add_service(UpdatesServiceServer::new(UpdatesApi(Arc::clone(&shared))))
        .add_service(NetworksServiceServer::new(NetworksApi(shared)))
}

fn bind(listen: Option<&Endpoint>) -> Result<Listener, DaemonError> {
    if let Some(endpoint) = listen {
        return Ok(Listener::bind(endpoint)?);
    }
    #[cfg(unix)]
    if let Some(listener) = Listener::from_systemd()? {
        return Ok(listener);
    }
    Ok(Listener::bind(&Endpoint::default_for_user()?)?)
}

/// The configured backend and, for WSL, access to the user's distributions.
/// The configured backend and, for WSL, access to the user's distributions.
#[cfg_attr(
    not(windows),
    allow(unused_variables, reason = "only the WSL backend reads the stored settings")
)]
fn backend(
    config: &Config,
    settings_file: Option<&std::path::Path>,
) -> Result<(Arc<dyn Backend>, Option<WslAccess>), DaemonError> {
    match config.backend {
        BackendChoice::Host => Ok((host_backend(config.engine_socket.clone())?, None)),
        #[cfg(windows)]
        BackendChoice::Wsl => wsl_backend(config.guest_dir.clone(), settings_file),
        #[cfg(feature = "fake")]
        BackendChoice::Fake => {
            let fixtures = match &config.fixtures {
                Some(path) => contr_testkit::Fixtures::from_file(path).map_err(|source| DaemonError::Fixtures {
                    path: path.clone(),
                    source,
                })?,
                None => contr_testkit::Fixtures::basic(),
            };
            Ok((Arc::new(contr_backend::FakeBackend::new(fixtures)), None))
        }
    }
}

#[cfg(target_os = "linux")]
#[allow(
    clippy::unnecessary_wraps,
    reason = "platforms without a host backend share the signature"
)]
fn host_backend(engine_socket: Option<PathBuf>) -> Result<Arc<dyn Backend>, DaemonError> {
    Ok(Arc::new(contr_backend_host::HostBackend::new(engine_socket)))
}

#[cfg(not(target_os = "linux"))]
fn host_backend(_engine_socket: Option<PathBuf>) -> Result<Arc<dyn Backend>, DaemonError> {
    Err(DaemonError::UnsupportedBackend("host"))
}

/// The WSL backend, with the distributions the user's settings integrate; unreadable settings integrate none.
#[cfg(windows)]
fn wsl_backend(
    guest_dir: Option<PathBuf>,
    settings_file: Option<&std::path::Path>,
) -> Result<(Arc<dyn Backend>, Option<WslAccess>), DaemonError> {
    let setup = |source| DaemonError::Backend { backend: "wsl", source };
    let guest_dir = match guest_dir {
        Some(dir) => dir,
        None => std::env::current_exe()
            .map_err(setup)?
            .parent()
            .map(|dir| dir.join("guest"))
            .ok_or_else(|| setup(std::io::Error::other("contrd has no parent directory")))?,
    };

    let stored = settings_file.map(|path| contr_settings::SettingsFile::new(path.to_path_buf()).load());
    let integrated = match stored {
        Some(Ok(stored)) => stored.wsl_integrations,
        Some(Err(error)) => {
            tracing::warn!(?error, "cannot read the settings; no WSL distribution is integrated");
            std::collections::BTreeSet::new()
        }
        None => std::collections::BTreeSet::new(),
    };

    let backend = Arc::new(contr_backend_wsl::WslBackend::new(guest_dir, integrated).map_err(setup)?);
    let wsl = WslAccess {
        integration: Arc::clone(&backend) as Arc<dyn contr_backend::WslIntegration>,
    };
    Ok((backend, Some(wsl)))
}

/// Bundled tools default to `cli` next to the executable; only Windows bundles them.
#[cfg(windows)]
fn cli_locations(cli_dir: Option<PathBuf>) -> Option<CliLocations> {
    let bundle = cli_dir.or_else(|| Some(std::env::current_exe().ok()?.parent()?.join("cli")))?;
    let installation = contr_platform::cli_tools::Installation::new(paths::cli_dir()?);

    Some(CliLocations { bundle, installation })
}

#[cfg(not(windows))]
#[allow(
    clippy::unnecessary_wraps,
    clippy::needless_pass_by_value,
    reason = "platforms that bundle CLI tools share the signature"
)]
fn cli_locations(_cli_dir: Option<PathBuf>) -> Option<CliLocations> {
    None
}

fn docker_context(config: &Config, kind: BackendKind) -> Option<DockerContext> {
    if !config.docker_context || kind == BackendKind::Fake {
        return None;
    }
    Some(DockerContext {
        config_dir: contr_platform::docker_context::config_dir()?,
        name: product::DOCKER_CONTEXT,
    })
}

fn write_discovery(path: PathBuf, endpoint: &Endpoint) -> Option<PathBuf> {
    let file = DiscoveryFile {
        pid: std::process::id(),
        version: product::VERSION.to_owned(),
        api_revision: contr_api::API_REVISION,
        endpoints: vec![endpoint.to_string()],
    };
    match file.write(&path) {
        Ok(()) => Some(path),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "cannot write the discovery file");
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn notify_ready() {
    if let Err(error) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
        tracing::debug!(%error, "cannot notify systemd");
    }
}

#[cfg(not(target_os = "linux"))]
fn notify_ready() {}

struct Health;

#[async_trait]
impl EngineHealth for Health {
    async fn ping(&self, connector: &EngineConnector) -> bool {
        contr_engine::ping(connector).await
    }
}
