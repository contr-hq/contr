use std::sync::Arc;

use contr_api::images::v1 as proto;
use contr_api::images::v1::images_service_server::ImagesService;
use contr_api::images::v1::watch_images_response::Event;
use contr_engine::Change;
use contr_platform::docker_credentials::{self, RegistryCredentials};
use futures_util::StreamExt;
use tonic::{Request, Response, Status};

use super::{ResponseStream, Shared, watch};
use crate::status;

pub(crate) struct ImagesApi(pub(crate) Arc<Shared>);

#[tonic::async_trait]
impl ImagesService for ImagesApi {
    type WatchImagesStream = ResponseStream<proto::WatchImagesResponse>;
    type PullImageStream = ResponseStream<proto::PullImageResponse>;
    type PushImageStream = ResponseStream<proto::PushImageResponse>;

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
        Ok(Response::new(self.0.until_stopping(watch(changes, |change| {
            proto::WatchImagesResponse {
                event: Some(match change {
                    Change::Snapshot(images) => Event::Snapshot(proto::ImageSnapshot { images }),
                    Change::Upsert(image) => Event::Upserted(image),
                    Change::Remove(id) => Event::RemovedId(id),
                }),
            }
        }))))
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
        let credentials = credentials(&self.0, &request.reference).await;
        let progress = handle
            .engine
            .pull_image(&request.reference, &request.platform, credentials);
        Ok(Response::new(
            progress.map(|event| event.map_err(status::from_engine)).boxed(),
        ))
    }

    async fn push_image(
        &self,
        request: Request<proto::PushImageRequest>,
    ) -> Result<Response<Self::PushImageStream>, Status> {
        let request = request.into_inner();
        if request.reference.trim().is_empty() {
            return Err(Status::invalid_argument("reference is empty"));
        }

        let handle = self.0.slot.get().await?;
        let credentials = credentials(&self.0, &request.reference).await;
        let progress = handle.engine.push_image(&request.reference, credentials);
        Ok(Response::new(
            progress.map(|event| event.map_err(status::from_engine)).boxed(),
        ))
    }

    async fn tag_image(
        &self,
        request: Request<proto::TagImageRequest>,
    ) -> Result<Response<proto::TagImageResponse>, Status> {
        let request = request.into_inner();
        if request.target.trim().is_empty() {
            return Err(Status::invalid_argument("target is empty"));
        }

        let handle = self.0.slot.get().await?;
        handle
            .engine
            .tag_image(&request.source, &request.target)
            .await
            .map_err(status::from_engine)?;
        handle.cache.refresh_images().await;

        Ok(Response::new(proto::TagImageResponse {}))
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

    async fn inspect_image(
        &self,
        request: Request<proto::InspectImageRequest>,
    ) -> Result<Response<proto::InspectImageResponse>, Status> {
        let handle = self.0.slot.get().await?;
        let response = handle
            .engine
            .inspect_image(&request.into_inner().reference)
            .await
            .map_err(status::from_engine)?;

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

/// Credentials `docker login` stored for the registry of `reference`; without them, or when they are unreadable, the
/// engine pulls and pushes anonymously.
async fn credentials(shared: &Shared, reference: &str) -> Option<RegistryCredentials> {
    let config_dir = shared
        .docker_context
        .as_ref()
        .map(|context| context.config_dir.clone())
        .or_else(contr_platform::docker_context::config_dir)?;
    let helper_dirs: Vec<_> = shared.cli.iter().map(|cli| cli.installation.bin_dir()).collect();
    let reference = reference.to_owned();

    let lookup = move || docker_credentials::for_reference(&config_dir, &reference, &helper_dirs);
    match tokio::task::spawn_blocking(lookup).await {
        Ok(Ok(credentials)) => credentials,
        Ok(Err(error)) => {
            tracing::warn!(%error, "cannot read the registry credentials; continuing without them");
            None
        }
        Err(_) => None,
    }
}
