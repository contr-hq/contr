use std::pin::Pin;
use std::sync::Arc;

use contr_api::containers::v1 as proto;
use contr_api::containers::v1::containers_service_server::ContainersService;
use contr_api::containers::v1::exec_request::Input;
use contr_api::containers::v1::exec_response::Output;
use contr_api::containers::v1::watch_containers_response::Event;
use contr_engine::{Change, Engine, ExecOutput, ExecSession};
use futures_util::StreamExt;
use futures_util::stream;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tonic::{Request, Response, Status, Streaming};

use super::{ResponseStream, Shared, watch};
use crate::status;

pub(crate) struct ContainersApi(pub(crate) Arc<Shared>);

#[tonic::async_trait]
impl ContainersService for ContainersApi {
    type WatchContainersStream = ResponseStream<proto::WatchContainersResponse>;
    type StreamLogsStream = ResponseStream<proto::StreamLogsResponse>;
    type StreamStatsStream = ResponseStream<proto::StreamStatsResponse>;
    type ExecStream = ResponseStream<proto::ExecResponse>;

    async fn list_containers(
        &self,
        _: Request<proto::ListContainersRequest>,
    ) -> Result<Response<proto::ListContainersResponse>, Status> {
        let handle = self.0.slot.get().await?;
        let containers = handle.engine.list_containers().await.map_err(status::from_engine)?;
        Ok(Response::new(proto::ListContainersResponse { containers }))
    }

    async fn watch_containers(
        &self,
        _: Request<proto::WatchContainersRequest>,
    ) -> Result<Response<Self::WatchContainersStream>, Status> {
        let changes = self.0.slot.get().await?.cache.watch_containers();
        Ok(Response::new(watch(changes, |change| proto::WatchContainersResponse {
            event: Some(match change {
                Change::Snapshot(containers) => Event::Snapshot(proto::ContainerSnapshot { containers }),
                Change::Upsert(container) => Event::Upserted(container),
                Change::Remove(id) => Event::RemovedId(id),
            }),
        })))
    }

    async fn inspect_container(
        &self,
        request: Request<proto::InspectContainerRequest>,
    ) -> Result<Response<proto::InspectContainerResponse>, Status> {
        let handle = self.0.slot.get().await?;
        let response = handle
            .engine
            .inspect_container(&request.into_inner().id)
            .await
            .map_err(status::from_engine)?;

        Ok(Response::new(response))
    }

    async fn start_container(
        &self,
        request: Request<proto::StartContainerRequest>,
    ) -> Result<Response<proto::StartContainerResponse>, Status> {
        let id = request.into_inner().id;
        let handle = self.0.slot.get().await?;
        handle.engine.start_container(&id).await.map_err(status::from_engine)?;
        refresh(&handle.cache, &id).await;

        Ok(Response::new(proto::StartContainerResponse {}))
    }

    async fn stop_container(
        &self,
        request: Request<proto::StopContainerRequest>,
    ) -> Result<Response<proto::StopContainerResponse>, Status> {
        let request = request.into_inner();
        let handle = self.0.slot.get().await?;
        handle
            .engine
            .stop_container(&request.id, request.timeout_seconds)
            .await
            .map_err(status::from_engine)?;
        refresh(&handle.cache, &request.id).await;

        Ok(Response::new(proto::StopContainerResponse {}))
    }

    async fn restart_container(
        &self,
        request: Request<proto::RestartContainerRequest>,
    ) -> Result<Response<proto::RestartContainerResponse>, Status> {
        let request = request.into_inner();
        let handle = self.0.slot.get().await?;
        handle
            .engine
            .restart_container(&request.id, request.timeout_seconds)
            .await
            .map_err(status::from_engine)?;
        refresh(&handle.cache, &request.id).await;

        Ok(Response::new(proto::RestartContainerResponse {}))
    }

    async fn remove_container(
        &self,
        request: Request<proto::RemoveContainerRequest>,
    ) -> Result<Response<proto::RemoveContainerResponse>, Status> {
        let request = request.into_inner();
        let handle = self.0.slot.get().await?;
        let id = handle.cache.resolve_container(&request.id);
        handle
            .engine
            .remove_container(&request.id, request.force, request.remove_volumes)
            .await
            .map_err(status::from_engine)?;
        if let Some(id) = id {
            handle.cache.refresh_container(&id).await;
        }

        Ok(Response::new(proto::RemoveContainerResponse {}))
    }

    async fn stream_logs(
        &self,
        request: Request<proto::StreamLogsRequest>,
    ) -> Result<Response<Self::StreamLogsStream>, Status> {
        let logs = self.0.slot.get().await?.engine.logs(&request.into_inner());
        Ok(Response::new(
            logs.map(|chunk| chunk.map_err(status::from_engine)).boxed(),
        ))
    }

    async fn stream_stats(
        &self,
        request: Request<proto::StreamStatsRequest>,
    ) -> Result<Response<Self::StreamStatsStream>, Status> {
        let request = request.into_inner();
        let samples = self.0.slot.get().await?.engine.stats(&request.id, request.one_sample);
        let responses = samples.map(|sample| {
            sample
                .map(|stats| proto::StreamStatsResponse { stats: Some(stats) })
                .map_err(status::from_engine)
        });

        Ok(Response::new(responses.boxed()))
    }

    async fn exec(
        &self,
        request: Request<Streaming<proto::ExecRequest>>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        let engine = self.0.slot.get().await?.engine.clone();
        let mut inbound = request.into_inner();
        let Some(proto::ExecRequest {
            input: Some(Input::Start(start)),
        }) = inbound.message().await?
        else {
            return Err(Status::invalid_argument("the first Exec message must be start"));
        };

        let ExecSession { id, output, input } = engine.exec(&start).await.map_err(status::from_engine)?;
        tokio::spawn(forward_input(inbound, input, engine.clone(), id.clone()));

        let started = stream::once(std::future::ready(Ok(response(Output::Started(proto::ExecStarted {
            exec_id: id.clone(),
        })))));
        let relayed = output.map(|chunk| {
            chunk
                .map(|chunk| match chunk {
                    ExecOutput::Stdout(bytes) => response(Output::Stdout(bytes.to_vec())),
                    ExecOutput::Stderr(bytes) => response(Output::Stderr(bytes.to_vec())),
                })
                .map_err(status::from_engine)
        });
        let exited = stream::once(async move {
            let exit_code = engine.exec_exit_code(&id).await.map_err(status::from_engine)?;
            Ok(response(Output::Exited(proto::ExecExited { exit_code })))
        });

        Ok(Response::new(started.chain(relayed).chain(exited).boxed()))
    }
}

fn response(output: Output) -> proto::ExecResponse {
    proto::ExecResponse { output: Some(output) }
}

async fn refresh(cache: &contr_engine::EngineCache, reference: &str) {
    if let Some(id) = cache.resolve_container(reference) {
        cache.refresh_container(&id).await;
    }
}

/// Relays client input until the client ends its request stream; stdin closes at `close_stdin` or at that end.
async fn forward_input(
    mut inbound: Streaming<proto::ExecRequest>,
    mut stdin: Pin<Box<dyn AsyncWrite + Send>>,
    engine: Engine,
    id: String,
) {
    let mut stdin_open = true;

    loop {
        let input = match inbound.message().await {
            Ok(Some(request)) => request.input,
            Ok(None) => break,
            Err(error) => {
                tracing::debug!(%error, "exec input stream failed");
                break;
            }
        };

        match input {
            Some(Input::Stdin(bytes)) if stdin_open => {
                if stdin.write_all(&bytes).await.and(stdin.flush().await).is_err() {
                    stdin_open = false;
                }
            }
            Some(Input::Resize(size)) => {
                if let Err(error) = engine.resize_exec(&id, size).await {
                    tracing::debug!(%error, "resizing the exec terminal failed");
                }
            }
            Some(Input::CloseStdin(_)) if stdin_open => {
                stdin_open = false;
                let _ = stdin.shutdown().await;
            }
            _ => {}
        }
    }

    if stdin_open {
        let _ = stdin.shutdown().await;
    }
}
