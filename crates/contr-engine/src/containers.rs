use std::collections::HashMap;
use std::str::FromStr;

use bollard::container::LogOutput;
use bollard::query_parameters::{
    CreateContainerOptions, InspectContainerOptions, KillContainerOptions, ListContainersOptions, LogsOptions,
    PruneContainersOptions, RemoveContainerOptions, RenameContainerOptions, RestartContainerOptions,
    StartContainerOptions, StatsOptions, StopContainerOptions,
};
use bytes::Bytes;
use contr_api::containers::v1::{
    Container, ContainerStats, CreateContainerRequest, CreateContainerResponse, InspectContainerResponse, LogStream,
    PruneContainersResponse, StreamLogsRequest, StreamLogsResponse,
};
use futures_util::{Stream, StreamExt};
use prost_types::Timestamp;

use crate::{Engine, EngineError, convert, create, stats};

impl Engine {
    /// Every container, stopped ones included.
    pub async fn list_containers(&self) -> Result<Vec<Container>, EngineError> {
        let options = ListContainersOptions {
            all: true,
            ..Default::default()
        };
        let summaries = self.docker().list_containers(Some(options)).await?;

        Ok(summaries.into_iter().map(convert::container).collect())
    }

    /// The container whose ID starts with `id`, if any.
    pub async fn find_container(&self, id: &str) -> Result<Option<Container>, EngineError> {
        let options = ListContainersOptions {
            all: true,
            filters: Some(HashMap::from([("id".to_owned(), vec![id.to_owned()])])),
            ..Default::default()
        };
        let summaries = self.docker().list_containers(Some(options)).await?;

        Ok(summaries.into_iter().next().map(convert::container))
    }

    /// Summary, details and the inspect document of one container.
    pub async fn inspect_container(&self, reference: &str) -> Result<InspectContainerResponse, EngineError> {
        let inspect = self
            .docker()
            .inspect_container(reference, None::<InspectContainerOptions>)
            .await?;
        let id = inspect.id.clone().unwrap_or_default();
        let container = self.find_container(&id).await?;
        let raw_json = serde_json::to_string(&inspect).map_err(|error| EngineError::Engine(error.to_string()))?;

        Ok(InspectContainerResponse {
            container,
            details: Some(convert::container_details(&inspect)),
            raw_json,
        })
    }

    pub async fn start_container(&self, reference: &str) -> Result<(), EngineError> {
        Ok(self
            .docker()
            .start_container(reference, None::<StartContainerOptions>)
            .await?)
    }

    pub async fn stop_container(&self, reference: &str, timeout_seconds: Option<i32>) -> Result<(), EngineError> {
        let options = StopContainerOptions {
            t: timeout_seconds,
            signal: None,
        };
        Ok(self.docker().stop_container(reference, Some(options)).await?)
    }

    pub async fn restart_container(&self, reference: &str, timeout_seconds: Option<i32>) -> Result<(), EngineError> {
        let options = RestartContainerOptions {
            t: timeout_seconds,
            signal: None,
        };
        Ok(self.docker().restart_container(reference, Some(options)).await?)
    }

    /// Creates the container `request` describes and starts it when asked; one that cannot start is removed with its
    /// anonymous volumes.
    pub async fn create_container(
        &self,
        request: &CreateContainerRequest,
    ) -> Result<CreateContainerResponse, EngineError> {
        let options = CreateContainerOptions {
            name: Some(request.name.clone()).filter(|name| !name.is_empty()),
            platform: request.platform.clone(),
        };
        let created = self
            .docker()
            .create_container(Some(options), create::definition(request)?)
            .await?;

        if request.start
            && let Err(error) = self.start_container(&created.id).await
        {
            if let Err(cleanup) = self.remove_container(&created.id, true, true).await {
                tracing::warn!(error = %cleanup, id = %created.id, "cannot remove a container that failed to start");
            }
            return Err(error);
        }

        Ok(CreateContainerResponse {
            id: created.id,
            warnings: created.warnings,
        })
    }

    /// Deletes every stopped container.
    pub async fn prune_containers(&self) -> Result<PruneContainersResponse, EngineError> {
        let response = self.docker().prune_containers(None::<PruneContainersOptions>).await?;

        Ok(PruneContainersResponse {
            deleted_ids: response.containers_deleted.unwrap_or_default(),
            reclaimed_bytes: response
                .space_reclaimed
                .and_then(|bytes| u64::try_from(bytes).ok())
                .unwrap_or_default(),
        })
    }

    pub async fn pause_container(&self, reference: &str) -> Result<(), EngineError> {
        Ok(self.docker().pause_container(reference).await?)
    }

    pub async fn unpause_container(&self, reference: &str) -> Result<(), EngineError> {
        Ok(self.docker().unpause_container(reference).await?)
    }

    /// Sends `signal`, or SIGKILL when it is empty.
    pub async fn kill_container(&self, reference: &str, signal: &str) -> Result<(), EngineError> {
        let options = Some(KillContainerOptions {
            signal: signal.to_owned(),
        })
        .filter(|options| !options.signal.is_empty());

        Ok(self.docker().kill_container(reference, options).await?)
    }

    pub async fn rename_container(&self, reference: &str, name: &str) -> Result<(), EngineError> {
        let options = RenameContainerOptions { name: name.to_owned() };
        Ok(self.docker().rename_container(reference, options).await?)
    }

    pub async fn remove_container(&self, reference: &str, force: bool, volumes: bool) -> Result<(), EngineError> {
        let options = RemoveContainerOptions {
            force,
            v: volumes,
            link: false,
        };
        Ok(self.docker().remove_container(reference, Some(options)).await?)
    }

    /// Log output as the request selects it; containers with a terminal report everything as stdout.
    pub fn logs(
        &self,
        request: &StreamLogsRequest,
    ) -> impl Stream<Item = Result<StreamLogsResponse, EngineError>> + Send + use<> {
        let options = LogsOptions {
            follow: request.follow,
            stdout: true,
            stderr: true,
            since: request
                .since
                .map_or(0, |since| i32::try_from(since.seconds).unwrap_or(i32::MAX)),
            until: 0,
            timestamps: request.timestamps,
            tail: request
                .tail_lines
                .map_or_else(|| "all".to_owned(), |lines| lines.to_string()),
        };
        let timestamps = request.timestamps;

        self.docker()
            .logs(&request.id, Some(options))
            .filter_map(move |output| {
                std::future::ready(match output {
                    Ok(output) => log_chunk(output, timestamps).map(Ok),
                    Err(error) => Some(Err(error.into())),
                })
            })
    }

    /// Resource usage samples; with `one_sample` a single sample whose CPU usage spans about one second.
    pub fn stats(
        &self,
        reference: &str,
        one_sample: bool,
    ) -> impl Stream<Item = Result<ContainerStats, EngineError>> + Send + use<> {
        let options = StatsOptions {
            stream: !one_sample,
            one_shot: false,
        };

        self.docker()
            .stats(reference, Some(options))
            .map(|sample| sample.map(|sample| stats::sample(&sample)).map_err(EngineError::from))
    }
}

fn log_chunk(output: LogOutput, timestamps: bool) -> Option<StreamLogsResponse> {
    let (stream, message) = match output {
        LogOutput::StdOut { message } | LogOutput::Console { message } => (LogStream::Stdout, message),
        LogOutput::StdErr { message } => (LogStream::Stderr, message),
        LogOutput::StdIn { .. } => return None,
    };
    let (timestamp, data) = if timestamps {
        split_timestamp(message)
    } else {
        (None, message)
    };

    Some(StreamLogsResponse {
        stream: stream.into(),
        data: data.to_vec(),
        timestamp,
    })
}

/// Splits the RFC 3339 prefix the engine writes before each line when timestamps are requested.
fn split_timestamp(message: Bytes) -> (Option<Timestamp>, Bytes) {
    let Some(space) = message.iter().position(|&byte| byte == b' ') else {
        return (None, message);
    };
    let parsed = std::str::from_utf8(&message[..space])
        .ok()
        .and_then(|text| Timestamp::from_str(text).ok());

    match parsed {
        Some(timestamp) => (Some(timestamp), message.slice(space + 1..)),
        None => (None, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_engine_timestamps() {
        let (timestamp, data) = split_timestamp(Bytes::from_static(b"2025-10-09T08:53:20.25Z hello world\n"));

        assert_eq!(
            timestamp.map(|t| (t.seconds, t.nanos)),
            Some((1_760_000_000, 250_000_000))
        );
        assert_eq!(&data[..], b"hello world\n");
    }

    #[test]
    fn keeps_lines_without_a_timestamp() {
        let (timestamp, data) = split_timestamp(Bytes::from_static(b"plain line\n"));

        assert_eq!(timestamp, None);
        assert_eq!(&data[..], b"plain line\n");
    }
}
