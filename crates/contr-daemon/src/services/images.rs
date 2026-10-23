use std::collections::HashMap;
use std::sync::Arc;

use contr_api::common::v1::Progress;
use contr_api::images::v1 as proto;
use contr_api::images::v1::images_service_server::ImagesService;
use contr_api::images::v1::watch_images_response::Event;
use contr_api::images::v1::{pull_image_response, push_image_response};
use contr_api::operations::v1::follow_operation_response::Event as OperationEvent;
use contr_api::operations::v1::{OperationRequest, operation_request};
use contr_engine::Change;
use contr_platform::docker_credentials::{self, RegistryCredentials};
use futures_util::StreamExt;
use tonic::{Request, Response, Status};

use super::{ResponseStream, Shared, follow_started, watch};
use crate::operations::{Admission, Started};
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
        let started = start_pull(&self.0, request.into_inner()).await?;
        let events = follow_started(
            &self.0,
            &started,
            true,
            |event| match event {
                OperationEvent::PullImage(event) => Some(event),
                _ => None,
            },
            |_| false,
        )?;

        Ok(Response::new(events))
    }

    async fn push_image(
        &self,
        request: Request<proto::PushImageRequest>,
    ) -> Result<Response<Self::PushImageStream>, Status> {
        let started = start_push(&self.0, request.into_inner()).await?;
        let events = follow_started(
            &self.0,
            &started,
            true,
            |event| match event {
                OperationEvent::PushImage(event) => Some(event),
                _ => None,
            },
            |_| false,
        )?;

        Ok(Response::new(events))
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

/// Starts pulling `request.reference`, or returns the running pull of the same reference and platform.
pub(super) async fn start_pull(shared: &Shared, request: proto::PullImageRequest) -> Result<Started, Status> {
    if request.reference.trim().is_empty() {
        return Err(Status::invalid_argument("reference is empty"));
    }
    let handle = shared.slot.get().await?;
    let credentials = credentials(shared, &request.reference).await;
    let key = format!("pull {} {}", request.reference, request.platform);
    let pulls = handle
        .engine
        .pull_image(&request.reference, &request.platform, credentials);
    let reference = request.reference.clone();
    let operation = OperationRequest {
        kind: Some(operation_request::Kind::PullImage(request)),
    };

    shared
        .operations
        .start(operation, true, Admission::Join(key), move |sink| async move {
            let mut pulls = std::pin::pin!(pulls);
            let mut layers = HashMap::new();
            while let Some(event) = pulls.next().await {
                let event = event.map_err(status::from_engine)?;
                if let Some(pull_image_response::Event::Layer(layer)) = &event.event {
                    layers.insert(layer.layer_id.clone(), (layer.current_bytes, layer.total_bytes));
                    sink.progress(pull_progress(&reference, &layers));
                }
                sink.emit(OperationEvent::PullImage(event));
            }
            Ok(())
        })
}

/// Starts pushing `request.reference`, or returns the running push of the same reference.
pub(super) async fn start_push(shared: &Shared, request: proto::PushImageRequest) -> Result<Started, Status> {
    if request.reference.trim().is_empty() {
        return Err(Status::invalid_argument("reference is empty"));
    }
    let handle = shared.slot.get().await?;
    let credentials = credentials(shared, &request.reference).await;
    let key = format!("push {}", request.reference);
    let pushes = handle.engine.push_image(&request.reference, credentials);
    let operation = OperationRequest {
        kind: Some(operation_request::Kind::PushImage(request)),
    };

    shared
        .operations
        .start(operation, true, Admission::Join(key), move |sink| async move {
            let mut pushes = std::pin::pin!(pushes);
            while let Some(event) = pushes.next().await {
                let event = event.map_err(status::from_engine)?;
                if let Some(push_image_response::Event::Status(line)) = &event.event {
                    sink.progress(Progress {
                        message: line.clone(),
                        fraction: None,
                    });
                }
                sink.emit(OperationEvent::PushImage(event));
            }
            Ok(())
        })
}

/// Bytes of the layers whose size the engine reported, as a share of their sizes.
fn pull_progress(reference: &str, layers: &HashMap<String, (u64, u64)>) -> Progress {
    let (current, total) = layers.values().filter(|(_, total)| *total > 0).fold(
        (0_u64, 0_u64),
        |(current, total), (layer_current, layer_total)| {
            (current + layer_current.min(layer_total), total + layer_total)
        },
    );
    #[expect(clippy::cast_precision_loss, reason = "a progress fraction needs no more precision")]
    let fraction = (total > 0).then(|| current as f32 / total as f32);

    Progress {
        message: format!("Pulling {reference}"),
        fraction,
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
