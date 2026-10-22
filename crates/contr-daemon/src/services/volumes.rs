use std::sync::Arc;

use contr_api::volumes::v1 as proto;
use contr_api::volumes::v1::volumes_service_server::VolumesService;
use contr_api::volumes::v1::watch_volumes_response::Event;
use contr_engine::Change;
use tonic::{Request, Response, Status};

use super::{ResponseStream, Shared, watch};
use crate::status;

pub(crate) struct VolumesApi(pub(crate) Arc<Shared>);

#[tonic::async_trait]
impl VolumesService for VolumesApi {
    type WatchVolumesStream = ResponseStream<proto::WatchVolumesResponse>;

    async fn list_volumes(
        &self,
        _: Request<proto::ListVolumesRequest>,
    ) -> Result<Response<proto::ListVolumesResponse>, Status> {
        let handle = self.0.slot.get().await?;
        let volumes = handle.engine.list_volumes().await.map_err(status::from_engine)?;
        Ok(Response::new(proto::ListVolumesResponse { volumes }))
    }

    async fn watch_volumes(
        &self,
        _: Request<proto::WatchVolumesRequest>,
    ) -> Result<Response<Self::WatchVolumesStream>, Status> {
        let changes = self.0.slot.get().await?.cache.watch_volumes();
        Ok(Response::new(self.0.until_stopping(watch(changes, |change| {
            proto::WatchVolumesResponse {
                event: Some(match change {
                    Change::Snapshot(volumes) => Event::Snapshot(proto::VolumeSnapshot { volumes }),
                    Change::Upsert(volume) => Event::Upserted(volume),
                    Change::Remove(name) => Event::RemovedName(name),
                }),
            }
        }))))
    }

    async fn inspect_volume(
        &self,
        request: Request<proto::InspectVolumeRequest>,
    ) -> Result<Response<proto::InspectVolumeResponse>, Status> {
        let handle = self.0.slot.get().await?;
        let response = handle
            .engine
            .inspect_volume(&request.into_inner().name)
            .await
            .map_err(status::from_engine)?;

        Ok(Response::new(response))
    }

    async fn create_volume(
        &self,
        request: Request<proto::CreateVolumeRequest>,
    ) -> Result<Response<proto::CreateVolumeResponse>, Status> {
        let handle = self.0.slot.get().await?;
        let volume = handle
            .engine
            .create_volume(&request.into_inner())
            .await
            .map_err(status::from_engine)?;
        handle.cache.refresh_volume(&volume.name).await;

        Ok(Response::new(proto::CreateVolumeResponse { volume: Some(volume) }))
    }

    async fn remove_volume(
        &self,
        request: Request<proto::RemoveVolumeRequest>,
    ) -> Result<Response<proto::RemoveVolumeResponse>, Status> {
        let request = request.into_inner();
        let handle = self.0.slot.get().await?;
        handle
            .engine
            .remove_volume(&request.name, request.force)
            .await
            .map_err(status::from_engine)?;
        handle.cache.refresh_volume(&request.name).await;

        Ok(Response::new(proto::RemoveVolumeResponse {}))
    }

    async fn prune_volumes(
        &self,
        request: Request<proto::PruneVolumesRequest>,
    ) -> Result<Response<proto::PruneVolumesResponse>, Status> {
        let handle = self.0.slot.get().await?;
        let response = handle
            .engine
            .prune_volumes(request.into_inner().all)
            .await
            .map_err(status::from_engine)?;
        for name in &response.deleted {
            handle.cache.refresh_volume(name).await;
        }

        Ok(Response::new(response))
    }
}
