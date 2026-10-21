use std::path::Path;
use std::time::Duration;

use bollard::query_parameters::DataUsageOptions;
use bollard::{API_DEFAULT_VERSION, Docker};
use contr_api::daemon::v1::EngineInfo;
use contr_backend::EngineConnector;
use serde_json::Value;

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

impl Engine {
    /// `docker version` and `docker info` as the engine reports them.
    pub async fn system_report(&self) -> Result<(Value, Value), EngineError> {
        let (version, info) = tokio::try_join!(self.docker.version(), self.docker.info())?;

        Ok((
            serde_json::to_value(version).unwrap_or_default(),
            serde_json::to_value(info).unwrap_or_default(),
        ))
    }

    /// CPUs and memory bytes of the machine the engine runs on.
    pub async fn capacity(&self) -> Result<(u32, u64), EngineError> {
        let info = self.docker.info().await?;
        let cpus = info.ncpu.and_then(|cpus| u32::try_from(cpus).ok()).unwrap_or_default();
        let memory = info
            .mem_total
            .and_then(|bytes| u64::try_from(bytes).ok())
            .unwrap_or_default();

        Ok((cpus, memory))
    }

    /// Directory, on the machine the engine runs on, that holds its data.
    pub async fn root_dir(&self) -> Result<String, EngineError> {
        Ok(self.docker.info().await?.docker_root_dir.unwrap_or_default())
    }

    /// Bytes that images, containers, volumes and the build cache take.
    pub async fn disk_usage(&self) -> Result<u64, EngineError> {
        let options = DataUsageOptions {
            verbose: false,
            ..Default::default()
        };
        let usage = self.docker.df(Some(options)).await?;
        let sizes = [
            usage.image_usage.and_then(|usage| usage.total_size),
            usage.container_usage.and_then(|usage| usage.total_size),
            usage.volume_usage.and_then(|usage| usage.total_size),
            usage.build_cache_usage.and_then(|usage| usage.total_size),
        ];

        Ok(sizes
            .into_iter()
            .flatten()
            .filter_map(|size| u64::try_from(size).ok())
            .sum())
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
