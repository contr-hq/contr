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
use contr_api::networks::v1::networks_service_server::NetworksServiceServer;
use contr_api::volumes::v1::volumes_service_server::VolumesServiceServer;
use contr_backend::{Backend, BackendKind, EngineConnector, EngineHealth, EngineSupervisor, SupervisorConfig};
use contr_core::{paths, product};
use contr_transport::{DiscoveryFile, Endpoint, Listener, TransportError};
use tonic::transport::Server;

use crate::services::{ContainersApi, DaemonApi, EngineApi, ImagesApi, NetworksApi, Shared, VolumesApi};
use crate::slot::{DockerContext, EngineSlot};

/// Backend the daemon runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendChoice {
    /// The engine installed on the Linux host.
    Host,
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
    let supervisor = EngineSupervisor::spawn(Arc::clone(&backend), Arc::new(Health), SupervisorConfig::default()).await;
    let slot = EngineSlot::spawn(&supervisor, docker_context(&config, backend.kind()));
    let shared = Arc::new(Shared {
        supervisor,
        slot,
        backend_kind: backend.kind(),
        capabilities: backend.capabilities(),
        started_at: SystemTime::now(),
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
