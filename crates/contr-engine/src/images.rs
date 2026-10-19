use std::collections::HashMap;

use bollard::models::CreateImageInfo;
use bollard::query_parameters::{CreateImageOptions, ListImagesOptions, PruneImagesOptions, RemoveImageOptions};
use contr_api::images::v1::pull_image_response::Event;
use contr_api::images::v1::{
    Image, LayerProgress, PruneImagesResponse, PullImageResponse, PulledImage, RemoveImageResponse,
};
use futures_util::{Stream, StreamExt};

use crate::{Engine, EngineError, convert};

impl Engine {
    /// Every top-level image.
    pub async fn list_images(&self) -> Result<Vec<Image>, EngineError> {
        let images = self.docker().list_images(None::<ListImagesOptions>).await?;
        Ok(images.into_iter().map(convert::image).collect())
    }

    /// Pulls `reference`, adding the `latest` tag when it names neither tag nor digest; dropping the stream cancels
    /// the pull.
    pub fn pull_image(
        &self,
        reference: &str,
        platform: &str,
    ) -> impl Stream<Item = Result<PullImageResponse, EngineError>> + Send + use<> {
        let docker = self.docker().clone();
        let reference = with_default_tag(reference);
        let options = CreateImageOptions {
            from_image: Some(reference.clone()),
            platform: platform.to_owned(),
            ..Default::default()
        };

        async_stream::try_stream! {
            let mut progress = docker.create_image(Some(options), None, None);
            while let Some(info) = progress.next().await {
                if let Some(event) = pull_event(info?)? {
                    yield PullImageResponse { event: Some(event) };
                }
            }

            let image = docker.inspect_image(&reference).await?;
            let pulled = PulledImage { reference, image_id: image.id.unwrap_or_default() };
            yield PullImageResponse { event: Some(Event::Pulled(pulled)) };
        }
    }

    pub async fn remove_image(
        &self,
        reference: &str,
        force: bool,
        keep_parents: bool,
    ) -> Result<RemoveImageResponse, EngineError> {
        let options = RemoveImageOptions {
            force,
            noprune: keep_parents,
            platforms: None,
        };
        let items = self.docker().remove_image(reference, Some(options), None).await?;

        Ok(RemoveImageResponse {
            untagged: items.iter().filter_map(|item| item.untagged.clone()).collect(),
            deleted: items.into_iter().filter_map(|item| item.deleted).collect(),
        })
    }

    /// Deletes untagged images, or with `all` every image no container uses.
    pub async fn prune_images(&self, all: bool) -> Result<PruneImagesResponse, EngineError> {
        let dangling = if all { "false" } else { "true" };
        let options = PruneImagesOptions {
            filters: Some(HashMap::from([("dangling".to_owned(), vec![dangling.to_owned()])])),
        };
        let response = self.docker().prune_images(Some(options)).await?;

        Ok(PruneImagesResponse {
            deleted: response
                .images_deleted
                .unwrap_or_default()
                .into_iter()
                .filter_map(|item| item.deleted.or(item.untagged))
                .collect(),
            reclaimed_bytes: response
                .space_reclaimed
                .and_then(|bytes| u64::try_from(bytes).ok())
                .unwrap_or_default(),
        })
    }
}

fn pull_event(info: CreateImageInfo) -> Result<Option<Event>, EngineError> {
    if let Some(detail) = info.error_detail {
        return Err(EngineError::Engine(detail.message.unwrap_or_default()));
    }

    let status = info.status.unwrap_or_default();
    let progress = info.progress_detail.unwrap_or_default();
    let event = match info.id {
        Some(layer_id) => Event::Layer(LayerProgress {
            layer_id,
            status,
            current_bytes: progress
                .current
                .and_then(|bytes| u64::try_from(bytes).ok())
                .unwrap_or_default(),
            total_bytes: progress
                .total
                .and_then(|bytes| u64::try_from(bytes).ok())
                .unwrap_or_default(),
        }),
        None if status.is_empty() => return Ok(None),
        None => Event::Status(status),
    };

    Ok(Some(event))
}

/// The engine pulls every tag of a repository when the reference has no tag; Contr pulls `latest` like the Docker
/// CLI does.
fn with_default_tag(reference: &str) -> String {
    let name = reference.rsplit('/').next().unwrap_or(reference);
    if reference.contains('@') || name.contains(':') {
        reference.to_owned()
    } else {
        format!("{reference}:latest")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_latest_only_without_tag_or_digest() {
        assert_eq!(with_default_tag("alpine"), "alpine:latest");
        assert_eq!(with_default_tag("alpine:3.22"), "alpine:3.22");
        assert_eq!(
            with_default_tag("localhost:5000/team/app"),
            "localhost:5000/team/app:latest"
        );
        assert_eq!(with_default_tag("alpine@sha256:abc"), "alpine@sha256:abc");
    }
}
