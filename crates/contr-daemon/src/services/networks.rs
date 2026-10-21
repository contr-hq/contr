use std::sync::Arc;

use contr_api::containers::v1::Protocol;
use contr_api::networks::v1 as proto;
use contr_api::networks::v1::networks_service_server::NetworksService;
use contr_api::networks::v1::watch_networks_response::Event;
use contr_engine::Change;
use contr_platform::ports::{self, Transport};
use tonic::{Request, Response, Status};

use super::{ResponseStream, Shared, watch};
use crate::status;

pub(crate) struct NetworksApi(pub(crate) Arc<Shared>);

#[tonic::async_trait]
impl NetworksService for NetworksApi {
    type WatchNetworksStream = ResponseStream<proto::WatchNetworksResponse>;

    async fn list_networks(
        &self,
        _: Request<proto::ListNetworksRequest>,
    ) -> Result<Response<proto::ListNetworksResponse>, Status> {
        let handle = self.0.slot.get().await?;
        let networks = handle.engine.list_networks().await.map_err(status::from_engine)?;
        Ok(Response::new(proto::ListNetworksResponse { networks }))
    }

    async fn watch_networks(
        &self,
        _: Request<proto::WatchNetworksRequest>,
    ) -> Result<Response<Self::WatchNetworksStream>, Status> {
        let changes = self.0.slot.get().await?.cache.watch_networks();
        Ok(Response::new(watch(changes, |change| proto::WatchNetworksResponse {
            event: Some(match change {
                Change::Snapshot(networks) => Event::Snapshot(proto::NetworkSnapshot { networks }),
                Change::Upsert(network) => Event::Upserted(network),
                Change::Remove(id) => Event::RemovedId(id),
            }),
        })))
    }

    async fn inspect_network(
        &self,
        request: Request<proto::InspectNetworkRequest>,
    ) -> Result<Response<proto::InspectNetworkResponse>, Status> {
        let handle = self.0.slot.get().await?;
        let response = handle
            .engine
            .inspect_network(&request.into_inner().id)
            .await
            .map_err(status::from_engine)?;

        Ok(Response::new(response))
    }

    async fn create_network(
        &self,
        request: Request<proto::CreateNetworkRequest>,
    ) -> Result<Response<proto::CreateNetworkResponse>, Status> {
        let request = request.into_inner();
        if request.name.trim().is_empty() {
            return Err(Status::invalid_argument("name is empty"));
        }

        let handle = self.0.slot.get().await?;
        let network = handle
            .engine
            .create_network(&request)
            .await
            .map_err(status::from_engine)?;
        handle.cache.refresh_networks().await;

        Ok(Response::new(proto::CreateNetworkResponse { network: Some(network) }))
    }

    async fn remove_network(
        &self,
        request: Request<proto::RemoveNetworkRequest>,
    ) -> Result<Response<proto::RemoveNetworkResponse>, Status> {
        let handle = self.0.slot.get().await?;
        handle
            .engine
            .remove_network(&request.into_inner().id)
            .await
            .map_err(status::from_engine)?;
        handle.cache.refresh_networks().await;

        Ok(Response::new(proto::RemoveNetworkResponse {}))
    }

    async fn connect_network(
        &self,
        request: Request<proto::ConnectNetworkRequest>,
    ) -> Result<Response<proto::ConnectNetworkResponse>, Status> {
        let request = request.into_inner();
        let handle = self.0.slot.get().await?;
        handle
            .engine
            .connect_network(&request.network, &request.container, &request.aliases)
            .await
            .map_err(status::from_engine)?;
        handle.cache.refresh_networks().await;

        Ok(Response::new(proto::ConnectNetworkResponse {}))
    }

    async fn disconnect_network(
        &self,
        request: Request<proto::DisconnectNetworkRequest>,
    ) -> Result<Response<proto::DisconnectNetworkResponse>, Status> {
        let request = request.into_inner();
        let handle = self.0.slot.get().await?;
        handle
            .engine
            .disconnect_network(&request.network, &request.container, request.force)
            .await
            .map_err(status::from_engine)?;
        handle.cache.refresh_networks().await;

        Ok(Response::new(proto::DisconnectNetworkResponse {}))
    }

    async fn prune_networks(
        &self,
        _: Request<proto::PruneNetworksRequest>,
    ) -> Result<Response<proto::PruneNetworksResponse>, Status> {
        let handle = self.0.slot.get().await?;
        let response = handle.engine.prune_networks().await.map_err(status::from_engine)?;
        handle.cache.refresh_networks().await;

        Ok(Response::new(response))
    }

    async fn check_host_ports(
        &self,
        request: Request<proto::CheckHostPortsRequest>,
    ) -> Result<Response<proto::CheckHostPortsResponse>, Status> {
        let ports = request.into_inner().ports;
        if let Some(invalid) = ports
            .iter()
            .find(|port| port.port == 0 || port.port > u32::from(u16::MAX))
        {
            return Err(Status::invalid_argument(format!("{} is not a port", invalid.port)));
        }

        let statuses = tokio::task::spawn_blocking(move || ports.into_iter().map(host_port_status).collect())
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(Response::new(proto::CheckHostPortsResponse { ports: statuses }))
    }
}

fn host_port_status(port: proto::HostPort) -> proto::HostPortStatus {
    let number = u16::try_from(port.port).unwrap_or_default();
    let in_use = match port.protocol() {
        Protocol::Unspecified | Protocol::Tcp => ports::in_use(number, Transport::Tcp),
        Protocol::Udp => ports::in_use(number, Transport::Udp),
        Protocol::Sctp => false,
    };

    proto::HostPortStatus {
        port: Some(port),
        in_use,
    }
}
