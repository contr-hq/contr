use std::path::Path;
use std::time::Duration;

use bollard::{API_DEFAULT_VERSION, Docker};
use contr_api::daemon::v1::EngineInfo;
use contr_backend::EngineConnector;

use crate::EngineError;

/// Seconds bollard waits for response headers; streamed bodies are not limited.
const REQUEST_TIMEOUT_SECONDS: u64 = 120;
const PING_TIMEOUT: Duration = Duration::from_secs(3);

/// Client of one engine's Docker API at the API version both sides support.
#[derive(Clone, Debug)]
pub struct Engine {
    docker: Docker,
    endpoint: String,
}

impl Engine {
    /// Connects to the API at `connector` and negotiates the API version.
    pub async fn connect(connector: &EngineConnector) -> Result<Self, EngineError> {
        let docker = open(connector)?.negotiate_version().await?;

        Ok(Self {
            docker,
            endpoint: docker_host(connector),
        })
    }

    /// Engine API address in `DOCKER_HOST` syntax.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(crate) fn docker(&self) -> &Docker {
        &self.docker
    }

    /// Version, platform and mode of the engine; `docker_context` is left empty for the caller.
    pub async fn info(&self) -> Result<EngineInfo, EngineError> {
        let (version, info) = tokio::try_join!(self.docker.version(), self.docker.info())?;
        let rootless = info
            .security_options
            .unwrap_or_default()
            .iter()
            .any(|option| option.split(',').any(|field| field == "name=rootless"));

        Ok(EngineInfo {
            endpoint: self.endpoint.clone(),
            docker_context: String::new(),
            version: version.version.unwrap_or_default(),
            api_version: version.api_version.unwrap_or_default(),
            os: version.os.unwrap_or_default(),
            arch: version.arch.unwrap_or_default(),
            kernel_version: version.kernel_version.unwrap_or_default(),
            rootless,
        })
    }
}

/// Returns `true` when the API at `connector` answers a ping within three seconds.
pub async fn ping(connector: &EngineConnector) -> bool {
    let Ok(docker) = open(connector) else {
        return false;
    };
    matches!(tokio::time::timeout(PING_TIMEOUT, docker.ping()).await, Ok(Ok(_)))
}

fn open(connector: &EngineConnector) -> Result<Docker, EngineError> {
    match connector {
        EngineConnector::UnixSocket(path) => open_unix(path),
        EngineConnector::NamedPipe(name) => open_pipe(name),
    }
}

#[cfg(unix)]
fn open_unix(path: &Path) -> Result<Docker, EngineError> {
    let path = path
        .to_str()
        .ok_or_else(|| EngineError::InvalidArgument(format!("{} is not UTF-8", path.display())))?;
    Ok(Docker::connect_with_unix(
        path,
        REQUEST_TIMEOUT_SECONDS,
        API_DEFAULT_VERSION,
    )?)
}

#[cfg(not(unix))]
fn open_unix(path: &Path) -> Result<Docker, EngineError> {
    Err(EngineError::InvalidArgument(format!(
        "{} is a Unix socket, which this platform cannot open",
        path.display()
    )))
}

#[cfg(windows)]
fn open_pipe(name: &str) -> Result<Docker, EngineError> {
    Ok(Docker::connect_with_named_pipe(
        name,
        REQUEST_TIMEOUT_SECONDS,
        API_DEFAULT_VERSION,
    )?)
}

#[cfg(not(windows))]
fn open_pipe(name: &str) -> Result<Docker, EngineError> {
    Err(EngineError::InvalidArgument(format!(
        "{name} is a named pipe, which exists only on Windows"
    )))
}

fn docker_host(connector: &EngineConnector) -> String {
    match connector {
        EngineConnector::UnixSocket(path) => format!("unix://{}", path.display()),
        EngineConnector::NamedPipe(name) => format!("npipe://{}", name.replace('\\', "/")),
    }
}
