//! The Contr daemon: composes a backend, its supervisor, the engine cache and the gRPC services, and serves them on
//! the per-user local endpoint.

mod services;
mod slot;
mod status;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use contr_api::containers::v1::containers_service_server::ContainersServiceServer;
use contr_api::daemon::v1::daemon_service_server::DaemonServiceServer;
use contr_api::engine::v1::engine_service_server::EngineServiceServer;
use contr_api::images::v1::images_service_server::ImagesServiceServer;
use contr_api::integration::v1::integration_service_server::IntegrationServiceServer;
use contr_api::networks::v1::networks_service_server::NetworksServiceServer;
use contr_api::volumes::v1::volumes_service_server::VolumesServiceServer;
use contr_backend::{Backend, BackendKind, EngineConnector, EngineHealth, EngineSupervisor, SupervisorConfig};
use contr_core::{paths, product};
use contr_transport::{DiscoveryFile, Endpoint, Listener, TransportError};
use tonic::transport::Server;

use crate::services::{
    CliLocations, ContainersApi, DaemonApi, EngineApi, ImagesApi, IntegrationApi, NetworksApi, Shared, VolumesApi,
};
use crate::slot::{DockerContext, EngineSlot};

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

/// Serves the API until `shutdown` completes.
pub async fn run(config: Config, shutdown: impl Future<Output = ()> + Send) -> Result<(), DaemonError> {
    let listener = bind(config.listen.as_ref())?;
    let backend = backend(&config)?;
    // Only the WSL backend owns its engine; a host engine its administrator stopped must stay stopped.
    let supervision = SupervisorConfig {
        recover: backend.kind() == BackendKind::Wsl,
        ..SupervisorConfig::default()
    };
    let supervisor = EngineSupervisor::spawn(Arc::clone(&backend), Arc::new(Health), supervision).await;
    let slot = EngineSlot::spawn(&supervisor, docker_context(&config, backend.kind()));
    let shared = Arc::new(Shared {
        supervisor,
        slot,
        backend_kind: backend.kind(),
        capabilities: backend.capabilities(),
        started_at: SystemTime::now(),
        cli: cli_locations(config.cli_dir.clone()),
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

    let served = Server::builder()
        .add_service(DaemonServiceServer::new(DaemonApi(Arc::clone(&shared))))
        .add_service(EngineServiceServer::new(EngineApi(Arc::clone(&shared))))
        .add_service(ContainersServiceServer::new(ContainersApi(Arc::clone(&shared))))
        .add_service(ImagesServiceServer::new(ImagesApi(Arc::clone(&shared))))
        .add_service(IntegrationServiceServer::new(IntegrationApi(Arc::clone(&shared))))
        .add_service(VolumesServiceServer::new(VolumesApi(Arc::clone(&shared))))
        .add_service(NetworksServiceServer::new(NetworksApi(shared)))
        .serve_with_incoming_shutdown(listener.incoming(), shutdown)
        .await;

    if let Some(path) = discovery {
        DiscoveryFile::remove(&path, std::process::id());
    }
    Ok(served?)
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

fn backend(config: &Config) -> Result<Arc<dyn Backend>, DaemonError> {
    match config.backend {
        BackendChoice::Host => host_backend(config.engine_socket.clone()),
        #[cfg(windows)]
        BackendChoice::Wsl => wsl_backend(config.guest_dir.clone(), config.default_pipe),
        #[cfg(feature = "fake")]
        BackendChoice::Fake => {
            let fixtures = match &config.fixtures {
                Some(path) => contr_testkit::Fixtures::from_file(path).map_err(|source| DaemonError::Fixtures {
                    path: path.clone(),
                    source,
                })?,
                None => contr_testkit::Fixtures::basic(),
            };
            Ok(Arc::new(contr_backend::FakeBackend::new(fixtures)))
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

#[cfg(windows)]
fn wsl_backend(guest_dir: Option<PathBuf>, default_pipe: bool) -> Result<Arc<dyn Backend>, DaemonError> {
    let setup = |source| DaemonError::Backend { backend: "wsl", source };
    let guest_dir = match guest_dir {
        Some(dir) => dir,
        None => std::env::current_exe()
            .map_err(setup)?
            .parent()
            .map(|dir| dir.join("guest"))
            .ok_or_else(|| setup(std::io::Error::other("contrd has no parent directory")))?,
    };

    Ok(Arc::new(
        contr_backend_wsl::WslBackend::new(guest_dir, default_pipe).map_err(setup)?,
    ))
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
        version: env!("CARGO_PKG_VERSION").to_owned(),
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
