use std::sync::Arc;

use contr_api::images::v1 as proto;
use contr_api::images::v1::images_service_server::ImagesService;
use contr_api::images::v1::watch_images_response::Event;
use contr_engine::Change;
use futures_util::StreamExt;
use tonic::{Request, Response, Status};

use super::{ResponseStream, Shared, watch};
use crate::status;

pub(crate) struct ImagesApi(pub(crate) Arc<Shared>);

#[tonic::async_trait]
impl ImagesService for ImagesApi {
    type WatchImagesStream = ResponseStream<proto::WatchImagesResponse>;
    type PullImageStream = ResponseStream<proto::PullImageResponse>;

    async fn list_images(
        &self,
        _: Request<proto::ListImagesRequest>,
    ) -> Result<Response<proto::ListImagesResponse>, Status> {
        let handle = self.0.slot.get().await?;
        let images = handle.engine.list_images().await.map_err(status::from_engine)?;
        Ok(Response::new(proto::ListImagesResponse { images }))
    }

    async fn watch_images(
        &self,
        _: Request<proto::WatchImagesRequest>,
    ) -> Result<Response<Self::WatchImagesStream>, Status> {
        let changes = self.0.slot.get().await?.cache.watch_images();
        Ok(Response::new(watch(changes, |change| proto::WatchImagesResponse {
            event: Some(match change {
                Change::Snapshot(images) => Event::Snapshot(proto::ImageSnapshot { images }),
                Change::Upsert(image) => Event::Upserted(image),
                Change::Remove(id) => Event::RemovedId(id),
            }),
        })))
    }

    async fn pull_image(
        &self,
        request: Request<proto::PullImageRequest>,
    ) -> Result<Response<Self::PullImageStream>, Status> {
        let request = request.into_inner();
        if request.reference.trim().is_empty() {
            return Err(Status::invalid_argument("reference is empty"));
        }

        let handle = self.0.slot.get().await?;
        let progress = handle.engine.pull_image(&request.reference, &request.platform);
        Ok(Response::new(
            progress.map(|event| event.map_err(status::from_engine)).boxed(),
        ))
    }

    async fn remove_image(
        &self,
        request: Request<proto::RemoveImageRequest>,
    ) -> Result<Response<proto::RemoveImageResponse>, Status> {
        let request = request.into_inner();
        let handle = self.0.slot.get().await?;
        let response = handle
            .engine
            .remove_image(&request.reference, request.force, request.keep_parents)
            .await
            .map_err(status::from_engine)?;
        handle.cache.refresh_images().await;

        Ok(Response::new(response))
    }

    async fn prune_images(
        &self,
        request: Request<proto::PruneImagesRequest>,
    ) -> Result<Response<proto::PruneImagesResponse>, Status> {
        let handle = self.0.slot.get().await?;
        let response = handle
            .engine
            .prune_images(request.into_inner().all)
            .await
            .map_err(status::from_engine)?;
        handle.cache.refresh_images().await;

        Ok(Response::new(response))
    }
}
