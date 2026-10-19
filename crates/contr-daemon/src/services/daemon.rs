use std::sync::Arc;

use contr_api::API_REVISION;
use contr_api::daemon::v1 as proto;
use contr_api::daemon::v1::daemon_service_server::DaemonService;
use contr_backend::{BackendKind, Capability};
use tonic::{Request, Response, Status};

use super::Shared;

const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) struct DaemonApi(pub(crate) Arc<Shared>);

#[tonic::async_trait]
impl DaemonService for DaemonApi {
    async fn handshake(
        &self,
        request: Request<proto::HandshakeRequest>,
    ) -> Result<Response<proto::HandshakeResponse>, Status> {
        let client = request.into_inner();
        tracing::debug!(
            client = client.client_name,
            version = client.client_version,
            revision = client.api_revision,
            "handshake"
        );

        Ok(Response::new(proto::HandshakeResponse {
            daemon_version: DAEMON_VERSION.to_owned(),
            api_revision: API_REVISION,
            backend_kind: backend_kind(self.0.backend_kind).into(),
            capabilities: self
                .0
                .capabilities
                .iter()
                .map(|&capability| capability_value(capability).into())
                .collect(),
        }))
    }

    async fn get_info(&self, _: Request<proto::GetInfoRequest>) -> Result<Response<proto::GetInfoResponse>, Status> {
        Ok(Response::new(proto::GetInfoResponse {
            daemon_version: DAEMON_VERSION.to_owned(),
            api_revision: API_REVISION,
            backend_kind: backend_kind(self.0.backend_kind).into(),
            pid: std::process::id(),
            started_at: Some(self.0.started_at.into()),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            engine: self.0.slot.info(),
        }))
    }
}

fn backend_kind(kind: BackendKind) -> proto::BackendKind {
    match kind {
        BackendKind::Host => proto::BackendKind::Host,
        BackendKind::Wsl => proto::BackendKind::Wsl,
        BackendKind::Qemu => proto::BackendKind::Qemu,
        BackendKind::Fake => proto::BackendKind::Fake,
    }
}

fn capability_value(capability: Capability) -> proto::Capability {
    match capability {
        Capability::Compose => proto::Capability::Compose,
        Capability::Kubernetes => proto::Capability::Kubernetes,
        Capability::ResourceLimits => proto::Capability::ResourceLimits,
        Capability::WslIntegration => proto::Capability::WslIntegration,
        Capability::HostPortForwarding => proto::Capability::HostPortForwarding,
    }
}
