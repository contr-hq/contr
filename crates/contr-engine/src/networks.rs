use bollard::models::{Ipam, IpamConfig, NetworkCreateRequest};
use bollard::query_parameters::{InspectNetworkOptions, ListNetworksOptions, PruneNetworksOptions};
use contr_api::networks::v1::{CreateNetworkRequest, InspectNetworkResponse, Network, PruneNetworksResponse};

use crate::{Engine, EngineError, convert};

impl Engine {
    pub async fn list_networks(&self) -> Result<Vec<Network>, EngineError> {
        let networks = self.docker().list_networks(None::<ListNetworksOptions>).await?;
        Ok(networks.into_iter().map(convert::network).collect())
    }

    pub async fn inspect_network(&self, reference: &str) -> Result<InspectNetworkResponse, EngineError> {
        let network = self
            .docker()
            .inspect_network(reference, None::<InspectNetworkOptions>)
            .await?;
        let raw_json = serde_json::to_string(&network).map_err(|error| EngineError::Engine(error.to_string()))?;

        Ok(InspectNetworkResponse {
            network: Some(convert::inspected_network(network)),
            raw_json,
        })
    }

    /// The network whose ID or name is `reference`, if it exists.
    pub async fn find_network(&self, reference: &str) -> Result<Option<Network>, EngineError> {
        match self.inspect_network(reference).await {
            Ok(response) => Ok(response.network),
            Err(EngineError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn create_network(&self, request: &CreateNetworkRequest) -> Result<Network, EngineError> {
        let ipam = (!request.subnets.is_empty()).then(|| Ipam {
            config: Some(
                request
                    .subnets
                    .iter()
                    .map(|subnet| IpamConfig {
                        subnet: Some(subnet.cidr.clone()),
                        gateway: Some(subnet.gateway.clone()).filter(|gateway| !gateway.is_empty()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        });
        let config = NetworkCreateRequest {
            name: request.name.clone(),
            driver: Some(request.driver.clone()).filter(|driver| !driver.is_empty()),
            internal: Some(request.internal),
            attachable: Some(request.attachable),
            enable_ipv6: Some(request.ipv6_enabled),
            labels: Some(request.labels.clone()),
            ipam,
            ..Default::default()
        };

        let id = self.docker().create_network(config).await?.id;
        self.find_network(&id)
            .await?
            .ok_or_else(|| EngineError::NotFound(format!("network {id} disappeared after creation")))
    }

    pub async fn remove_network(&self, reference: &str) -> Result<(), EngineError> {
        Ok(self.docker().remove_network(reference).await?)
    }

    /// Deletes user-defined networks no container is connected to.
    pub async fn prune_networks(&self) -> Result<PruneNetworksResponse, EngineError> {
        let response = self.docker().prune_networks(None::<PruneNetworksOptions>).await?;

        Ok(PruneNetworksResponse {
            deleted: response.networks_deleted.unwrap_or_default(),
        })
    }
}
