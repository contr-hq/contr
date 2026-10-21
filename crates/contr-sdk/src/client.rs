use std::future::Future;
use std::time::Duration;

use contr_api::API_REVISION;
use contr_api::containers::v1::containers_service_client::ContainersServiceClient;
use contr_api::containers::v1::{Container, ExecStart, ListContainersRequest};
use contr_api::daemon::v1::daemon_service_client::DaemonServiceClient;
use contr_api::daemon::v1::{GetInfoRequest, GetInfoResponse, HandshakeRequest, HandshakeResponse};
use contr_api::diagnostics::v1::diagnostics_service_client::DiagnosticsServiceClient;
use contr_api::engine::v1::engine_service_client::EngineServiceClient;
use contr_api::engine::v1::{WatchStateRequest, WatchStateResponse};
use contr_api::images::v1::images_service_client::ImagesServiceClient;
use contr_api::integration::v1::integration_service_client::IntegrationServiceClient;
use contr_api::networks::v1::networks_service_client::NetworksServiceClient;
use contr_api::settings::v1::settings_service_client::SettingsServiceClient;
use contr_api::volumes::v1::volumes_service_client::VolumesServiceClient;
use contr_core::product;
use contr_transport::Endpoint;
use tonic::Code;
use tonic::transport::Channel;

use crate::{Error, ExecSession};

const IDEMPOTENT_ATTEMPTS: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_millis(200);

/// Name and version a client reports in the handshake; the daemon logs them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientIdentity {
    /// Program name.
    pub name: String,
    /// Program version.
    pub version: String,
}

impl Default for ClientIdentity {
    fn default() -> Self {
        Self {
            name: format!("{}-sdk", product::SLUG),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// Connection to a daemon that completed the handshake. Clones share the connection.
#[derive(Clone, Debug)]
pub struct Client {
    channel: Channel,
    handshake: HandshakeResponse,
}

impl Client {
    /// Connects to `endpoint` and performs the handshake.
    pub async fn connect(endpoint: &Endpoint, identity: &ClientIdentity) -> Result<Self, Error> {
        let channel = contr_transport::connect(endpoint)
            .await
            .map_err(|source| Error::connect(endpoint, source))?;

        let request = HandshakeRequest {
            client_name: identity.name.clone(),
            client_version: identity.version.clone(),
            api_revision: API_REVISION,
        };
        let handshake = DaemonServiceClient::new(channel.clone())
            .handshake(request)
            .await
            .map_err(|status| match status.code() {
                Code::Unimplemented => Error::VersionMismatch(format!(
                    "the server at {endpoint} is not a {} daemon",
                    product::DISPLAY_NAME
                )),
                _ => Error::from(status),
            })?
            .into_inner();

        Ok(Self { channel, handshake })
    }

    /// Connects to the endpoint [`Endpoint::resolve`] selects.
    pub async fn connect_default(identity: &ClientIdentity) -> Result<Self, Error> {
        let endpoint = Endpoint::resolve().map_err(Error::Endpoint)?;
        Self::connect(&endpoint, identity).await
    }

    /// Connects to the resolved endpoint, starting the daemon first when nothing answers there and `CONTR_HOST` is
    /// unset.
    pub async fn connect_or_start(identity: &ClientIdentity) -> Result<Self, Error> {
        crate::start::connect_or_start(identity).await
    }

    /// The daemon's handshake answer, including its API revision and capabilities.
    pub fn handshake(&self) -> &HandshakeResponse {
        &self.handshake
    }

    /// Daemon identity service.
    pub fn daemon(&self) -> DaemonServiceClient<Channel> {
        DaemonServiceClient::new(self.channel.clone())
    }

    /// Engine lifecycle service.
    pub fn engine(&self) -> EngineServiceClient<Channel> {
        EngineServiceClient::new(self.channel.clone())
    }

    /// Containers service.
    pub fn containers(&self) -> ContainersServiceClient<Channel> {
        ContainersServiceClient::new(self.channel.clone())
    }

    /// Images service.
    pub fn images(&self) -> ImagesServiceClient<Channel> {
        ImagesServiceClient::new(self.channel.clone())
    }

    /// Integration with the user's own tools.
    pub fn integration(&self) -> IntegrationServiceClient<Channel> {
        IntegrationServiceClient::new(self.channel.clone())
    }

    /// Checks of what keeps the engine from working, and the fixes the daemon runs.
    pub fn diagnostics(&self) -> DiagnosticsServiceClient<Channel> {
        DiagnosticsServiceClient::new(self.channel.clone())
    }

    /// The user's settings and the ones an administrator fixes.
    pub fn settings(&self) -> SettingsServiceClient<Channel> {
        SettingsServiceClient::new(self.channel.clone())
    }

    /// Volumes service.
    pub fn volumes(&self) -> VolumesServiceClient<Channel> {
        VolumesServiceClient::new(self.channel.clone())
    }

    /// Networks service.
    pub fn networks(&self) -> NetworksServiceClient<Channel> {
        NetworksServiceClient::new(self.channel.clone())
    }

    /// Daemon and engine description.
    pub async fn info(&self) -> Result<GetInfoResponse, Error> {
        retry(|| async {
            self.daemon()
                .get_info(GetInfoRequest {})
                .await
                .map(tonic::Response::into_inner)
        })
        .await
    }

    /// Current engine state.
    pub async fn engine_state(&self) -> Result<WatchStateResponse, Error> {
        retry(|| async {
            let mut states = self.engine().watch_state(WatchStateRequest {}).await?.into_inner();
            states
                .message()
                .await?
                .ok_or_else(|| tonic::Status::unavailable("the state stream ended without a state"))
        })
        .await
    }

    /// Every container, stopped ones included.
    pub async fn list_containers(&self) -> Result<Vec<Container>, Error> {
        let response = retry(|| async {
            self.containers()
                .list_containers(ListContainersRequest {})
                .await
                .map(tonic::Response::into_inner)
        })
        .await?;
        Ok(response.containers)
    }

    /// Starts `start` in its container and returns once the command runs.
    pub async fn exec(&self, start: ExecStart) -> Result<ExecSession, Error> {
        ExecSession::start(self.containers(), start).await
    }
}

/// Retries an idempotent call while the daemon is momentarily unavailable.
async fn retry<T, F>(call: impl Fn() -> F) -> Result<T, Error>
where
    F: Future<Output = Result<T, tonic::Status>>,
{
    let mut attempt = 1;
    loop {
        match call().await {
            Ok(response) => return Ok(response),
            Err(status)
                if status.code() == Code::Unavailable
                    && status.details().is_empty()
                    && attempt < IDEMPOTENT_ATTEMPTS =>
            {
                attempt += 1;
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(status) => return Err(status.into()),
        }
    }
}
