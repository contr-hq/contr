use std::collections::HashMap;

use bollard::models::VolumeCreateRequest;
use bollard::query_parameters::{ListVolumesOptions, PruneVolumesOptions, RemoveVolumeOptions};
use contr_api::volumes::v1::{CreateVolumeRequest, InspectVolumeResponse, PruneVolumesResponse, Volume};

use crate::{Engine, EngineError, convert};

impl Engine {
    pub async fn list_volumes(&self) -> Result<Vec<Volume>, EngineError> {
        let response = self.docker().list_volumes(None::<ListVolumesOptions>).await?;
        Ok(response
            .volumes
            .unwrap_or_default()
            .into_iter()
            .map(convert::volume)
            .collect())
    }

    pub async fn inspect_volume(&self, name: &str) -> Result<InspectVolumeResponse, EngineError> {
        let volume = self.docker().inspect_volume(name).await?;
        let raw_json = serde_json::to_string(&volume).map_err(|error| EngineError::Engine(error.to_string()))?;

        Ok(InspectVolumeResponse {
            volume: Some(convert::volume(volume)),
            raw_json,
        })
    }

    /// The volume named `name`, if it exists.
    pub async fn find_volume(&self, name: &str) -> Result<Option<Volume>, EngineError> {
        match self.docker().inspect_volume(name).await {
            Ok(volume) => Ok(Some(convert::volume(volume))),
            Err(error) => match EngineError::from(error) {
                EngineError::NotFound(_) => Ok(None),
                error => Err(error),
            },
        }
    }

    pub async fn create_volume(&self, request: &CreateVolumeRequest) -> Result<Volume, EngineError> {
        let config = VolumeCreateRequest {
            name: Some(request.name.clone()).filter(|name| !name.is_empty()),
            driver: Some(request.driver.clone()).filter(|driver| !driver.is_empty()),
            driver_opts: Some(request.driver_options.clone()),
            labels: Some(request.labels.clone()),
            cluster_volume_spec: None,
        };
        let volume = self.docker().create_volume(config).await?;

        Ok(convert::volume(volume))
    }

    pub async fn remove_volume(&self, name: &str, force: bool) -> Result<(), EngineError> {
        Ok(self
            .docker()
            .remove_volume(name, Some(RemoveVolumeOptions { force }))
            .await?)
    }

    /// Deletes unused anonymous volumes, or with `all` every unused volume.
    pub async fn prune_volumes(&self, all: bool) -> Result<PruneVolumesResponse, EngineError> {
        let filters = all.then(|| HashMap::from([("all".to_owned(), vec!["true".to_owned()])]));
        let response = self
            .docker()
            .prune_volumes(Some(PruneVolumesOptions { filters }))
            .await?;

        Ok(PruneVolumesResponse {
            deleted: response.volumes_deleted.unwrap_or_default(),
            reclaimed_bytes: response
                .space_reclaimed
                .and_then(|bytes| u64::try_from(bytes).ok())
                .unwrap_or_default(),
        })
    }
}
