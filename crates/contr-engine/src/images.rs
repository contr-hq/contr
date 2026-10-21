use std::collections::HashMap;

use bollard::auth::DockerCredentials;
use bollard::models::{CreateImageInfo, ProgressDetail, PushImageInfo};
use bollard::query_parameters::{
    CreateImageOptions, ListImagesOptions, PruneImagesOptions, PushImageOptions, RemoveImageOptions, TagImageOptions,
};
use contr_api::images::v1::pull_image_response::Event;
use contr_api::images::v1::{
    Image, InspectImageResponse, LayerProgress, PruneImagesResponse, PullImageResponse, PulledImage, PushImageResponse,
    PushedImage, RemoveImageResponse, push_image_response,
};
use contr_platform::docker_credentials::RegistryCredentials;
use futures_util::{Stream, StreamExt};

use crate::{Engine, EngineError, convert};

impl Engine {
    /// Every top-level image.
    pub async fn list_images(&self) -> Result<Vec<Image>, EngineError> {
        let images = self.docker().list_images(None::<ListImagesOptions>).await?;
        Ok(images.into_iter().map(convert::image).collect())
    }

    /// Summary, details and the inspect document of one local image.
    pub async fn inspect_image(&self, reference: &str) -> Result<InspectImageResponse, EngineError> {
        let inspect = self.docker().inspect_image(reference).await?;
        let raw_json = serde_json::to_string(&inspect).map_err(|error| EngineError::Engine(error.to_string()))?;
        let (image, details) = convert::image_inspect(inspect);

        Ok(InspectImageResponse {
            image: Some(image),
            details: Some(details),
            raw_json,
        })
    }

    /// Pulls `reference` with `credentials` for its registry, adding the `latest` tag when it names neither tag nor
    /// digest; dropping the stream cancels the pull.
    pub fn pull_image(
        &self,
        reference: &str,
        platform: &str,
        credentials: Option<RegistryCredentials>,
    ) -> impl Stream<Item = Result<PullImageResponse, EngineError>> + Send + use<> {
        let docker = self.docker().clone();
        let reference = with_default_tag(reference);
        let options = CreateImageOptions {
            from_image: Some(reference.clone()),
            platform: platform.to_owned(),
            ..Default::default()
        };

        async_stream::try_stream! {
            let mut progress = docker.create_image(Some(options), None, credentials.map(docker_credentials));
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

    /// Pushes `reference` with `credentials` for its registry, adding the `latest` tag when it names none; dropping
    /// the stream cancels the push.
    pub fn push_image(
        &self,
        reference: &str,
        credentials: Option<RegistryCredentials>,
    ) -> impl Stream<Item = Result<PushImageResponse, EngineError>> + Send + use<> {
        let docker = self.docker().clone();
        let reference = with_default_tag(reference);
        let (repository, tag) = split_tag(&reference);
        let repository = repository.to_owned();
        let options = PushImageOptions {
            tag: tag.map(str::to_owned),
            platform: None,
        };

        async_stream::try_stream! {
            let mut digest = String::new();
            let mut progress = docker.push_image(&repository, Some(options), credentials.map(docker_credentials));
            while let Some(info) = progress.next().await {
                let status = push_status(info?)?;
                if let Some(found) = digest_in(&status) {
                    found.clone_into(&mut digest);
                }
                if !status.is_empty() {
                    yield PushImageResponse { event: Some(push_image_response::Event::Status(status)) };
                }
            }

            let pushed = PushedImage { reference, digest };
            yield PushImageResponse { event: Some(push_image_response::Event::Pushed(pushed)) };
        }
    }

    /// Adds `target`, with the `latest` tag when it names none, to the local image `source`.
    pub async fn tag_image(&self, source: &str, target: &str) -> Result<(), EngineError> {
        let target = with_default_tag(target);
        let (repository, tag) = split_tag(&target);
        let options = TagImageOptions {
            repo: Some(repository.to_owned()),
            tag: tag.map(str::to_owned),
        };

        Ok(self.docker().tag_image(source, Some(options)).await?)
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

fn docker_credentials(credentials: RegistryCredentials) -> DockerCredentials {
    DockerCredentials {
        username: credentials.username,
        password: credentials.password,
        identitytoken: credentials.identity_token,
        serveraddress: Some(credentials.server),
        ..Default::default()
    }
}

/// The engine's status line with the bytes of its progress, such as "Pushing 12.3 MB of 40.1 MB".
fn push_status(info: PushImageInfo) -> Result<String, EngineError> {
    if let Some(detail) = info.error_detail {
        return Err(EngineError::Engine(detail.message.unwrap_or_default()));
    }

    let status = info.status.unwrap_or_default();
    Ok(match info.progress_detail.unwrap_or_default() {
        ProgressDetail {
            current: Some(current),
            total: Some(total),
        } if total > 0 => format!("{status} {} of {}", megabytes(current), megabytes(total)),
        _ => status,
    })
}

fn megabytes(bytes: i64) -> String {
    let tenths = bytes / 100_000;
    format!("{}.{} MB", tenths / 10, tenths % 10)
}

/// The digest in the engine's closing status line, such as "1.2: digest: sha256:0123 size: 1570".
fn digest_in(status: &str) -> Option<&str> {
    status.split_once("digest: ")?.1.split_whitespace().next()
}

/// `repository:tag` split at the tag's colon, which follows the last slash.
fn split_tag(reference: &str) -> (&str, Option<&str>) {
    let name_start = reference.rfind('/').map_or(0, |slash| slash + 1);
    match reference[name_start..].rfind(':') {
        Some(colon) => (
            &reference[..name_start + colon],
            Some(&reference[name_start + colon + 1..]),
        ),
        None => (reference, None),
    }
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

    #[test]
    fn splits_the_tag_after_the_last_slash() {
        assert_eq!(split_tag("alpine:3.22"), ("alpine", Some("3.22")));
        assert_eq!(
            split_tag("registry.corp:5000/team/app:1.2"),
            ("registry.corp:5000/team/app", Some("1.2"))
        );
        assert_eq!(
            split_tag("registry.corp:5000/team/app"),
            ("registry.corp:5000/team/app", None)
        );
    }

    #[test]
    fn reports_push_progress_and_the_digest() {
        let pushing = PushImageInfo {
            status: Some("Pushing".to_owned()),
            progress_detail: Some(ProgressDetail {
                current: Some(12_345_678),
                total: Some(40_100_000),
            }),
            ..Default::default()
        };
        let closing = "1.2: digest: sha256:0123abcd size: 1570";

        assert_eq!(push_status(pushing).unwrap(), "Pushing 12.3 MB of 40.1 MB");
        assert_eq!(digest_in(closing), Some("sha256:0123abcd"));
        assert_eq!(digest_in("Layer already exists"), None);
    }
}
