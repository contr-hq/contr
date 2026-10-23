use std::sync::Arc;

use contr_api::common::v1::Progress;
use contr_api::engine::v1 as proto;
use contr_api::engine::v1::engine_service_server::EngineService;
use contr_api::engine::v1::{reset_response, restart_response, start_response, stop_response};
use contr_api::operations::v1::OperationRequest;
use contr_api::operations::v1::follow_operation_response::Event;
use contr_api::operations::v1::operation_request::Kind;
use contr_backend::{EngineState, OperationEvent, ResetScope};
use futures_util::stream::BoxStream;
use futures_util::{Stream, StreamExt};
use tokio_stream::wrappers::WatchStream;
use tonic::{Request, Response, Status};

use super::{ResponseStream, Shared, follow_started, watch};
use crate::operations::{Admission, Started};
use crate::slot::EngineSlot;
use crate::status;

pub(crate) struct EngineApi(pub(crate) Arc<Shared>);

#[tonic::async_trait]
impl EngineService for EngineApi {
    type WatchStateStream = ResponseStream<proto::WatchStateResponse>;
    type StartStream = ResponseStream<proto::StartResponse>;
    type StopStream = ResponseStream<proto::StopResponse>;
    type RestartStream = ResponseStream<proto::RestartResponse>;
    type ResetStream = ResponseStream<proto::ResetResponse>;
    type StreamUsageStream = ResponseStream<proto::StreamUsageResponse>;

    async fn watch_state(
        &self,
        _: Request<proto::WatchStateRequest>,
    ) -> Result<Response<Self::WatchStateStream>, Status> {
        let states = WatchStream::new(self.0.supervisor.subscribe()).map(|status| {
            let (state, failure_reason) = engine_state(status.state());
            Ok(proto::WatchStateResponse {
                state: state.into(),
                failure_reason: failure_reason.into(),
            })
        });

        Ok(Response::new(self.0.until_stopping(states.boxed())))
    }

    async fn start(&self, _: Request<proto::StartRequest>) -> Result<Response<Self::StartStream>, Status> {
        let started = start_engine(&self.0, Kind::StartEngine(proto::StartRequest {}))?;
        Ok(Response::new(follow_started(
            &self.0,
            &started,
            false,
            |event| {
                engine_event(event).map(|event| proto::StartResponse {
                    event: Some(match event {
                        Either::Progress(progress) => start_response::Event::Progress(progress),
                        Either::State(state) => start_response::Event::State(state),
                    }),
                })
            },
            |_| false,
        )?))
    }

    async fn stop(&self, _: Request<proto::StopRequest>) -> Result<Response<Self::StopStream>, Status> {
        let started = start_engine(&self.0, Kind::StopEngine(proto::StopRequest {}))?;
        Ok(Response::new(follow_started(
            &self.0,
            &started,
            false,
            |event| {
                engine_event(event).map(|event| proto::StopResponse {
                    event: Some(match event {
                        Either::Progress(progress) => stop_response::Event::Progress(progress),
                        Either::State(state) => stop_response::Event::State(state),
                    }),
                })
            },
            |_| false,
        )?))
    }

    async fn restart(&self, _: Request<proto::RestartRequest>) -> Result<Response<Self::RestartStream>, Status> {
        let started = start_engine(&self.0, Kind::RestartEngine(proto::RestartRequest {}))?;
        Ok(Response::new(follow_started(
            &self.0,
            &started,
            false,
            |event| {
                engine_event(event).map(|event| proto::RestartResponse {
                    event: Some(match event {
                        Either::Progress(progress) => restart_response::Event::Progress(progress),
                        Either::State(state) => restart_response::Event::State(state),
                    }),
                })
            },
            |_| false,
        )?))
    }

    async fn reset(&self, request: Request<proto::ResetRequest>) -> Result<Response<Self::ResetStream>, Status> {
        let started = start_engine(&self.0, Kind::ResetEngine(request.into_inner()))?;
        Ok(Response::new(follow_started(
            &self.0,
            &started,
            false,
            |event| {
                engine_event(event).map(|event| proto::ResetResponse {
                    event: Some(match event {
                        Either::Progress(progress) => reset_response::Event::Progress(progress),
                        Either::State(state) => reset_response::Event::State(state),
                    }),
                })
            },
            |_| false,
        )?))
    }

    async fn stream_usage(
        &self,
        _: Request<proto::StreamUsageRequest>,
    ) -> Result<Response<Self::StreamUsageStream>, Status> {
        let usage = self.0.slot.get().await?.cache.watch_usage();
        Ok(Response::new(self.0.until_stopping(watch(usage, |usage| {
            proto::StreamUsageResponse { usage: Some(usage) }
        }))))
    }
}

/// Starts the engine operation `kind` names, or returns the running one of the same kind and scope; later ones queue
/// behind it in the supervisor.
pub(super) fn start_engine(shared: &Arc<Shared>, kind: Kind) -> Result<Started, Status> {
    let key = match &kind {
        Kind::StartEngine(_) => "engine start".to_owned(),
        Kind::StopEngine(_) => "engine stop".to_owned(),
        Kind::RestartEngine(_) => "engine restart".to_owned(),
        Kind::ResetEngine(request) => match request.scope() {
            proto::ResetScope::Unspecified => return Err(Status::invalid_argument("a reset scope is required")),
            scope => format!("engine reset {}", scope.as_str_name()),
        },
        _ => return Err(Status::invalid_argument("not an engine operation")),
    };
    let supervisor = shared.supervisor.clone();
    let slot = shared.slot.clone();
    let operation = OperationRequest {
        kind: Some(kind.clone()),
    };

    shared
        .operations
        .start(operation, false, Admission::Join(key), move |sink| async move {
            let events = match kind {
                Kind::StopEngine(_) => supervisor.stop().await.boxed(),
                Kind::RestartEngine(_) => until_ready(slot, supervisor.restart().await),
                Kind::ResetEngine(request) => {
                    let scope = match request.scope() {
                        proto::ResetScope::Factory => ResetScope::Factory,
                        _ => ResetScope::Runtime,
                    };
                    supervisor.reset(scope).await.boxed()
                }
                _ => until_ready(slot, supervisor.start().await),
            };
            let mut events = std::pin::pin!(events);
            while let Some(event) = events.next().await {
                match event {
                    OperationEvent::Progress(step) => {
                        let progress = Progress {
                            message: step.message,
                            fraction: step.fraction,
                        };
                        sink.progress(progress.clone());
                        sink.emit(Event::EngineProgress(progress));
                    }
                    OperationEvent::Finished(Ok(state)) => {
                        sink.emit(Event::EngineState(engine_state(state).0.into()));
                        return Ok(());
                    }
                    OperationEvent::Finished(Err(error)) => return Err(status::from_operation(&error)),
                }
            }
            Err(Status::internal(
                "the engine supervisor ended the operation without a result",
            ))
        })
}

/// Holds back a successful result until the engine handle is ready, so calls made after it find the engine.
fn until_ready(
    slot: EngineSlot,
    events: impl Stream<Item = OperationEvent> + Send + 'static,
) -> BoxStream<'static, OperationEvent> {
    events
        .then(move |event| {
            let slot = slot.clone();
            async move {
                if matches!(event, OperationEvent::Finished(Ok(EngineState::Running))) {
                    let _ = slot.get().await;
                }
                event
            }
        })
        .boxed()
}

enum Either {
    Progress(Progress),
    State(i32),
}

/// An engine operation's event as the engine RPCs send it.
fn engine_event(event: Event) -> Option<Either> {
    match event {
        Event::EngineProgress(progress) => Some(Either::Progress(progress)),
        Event::EngineState(state) => Some(Either::State(state)),
        _ => None,
    }
}

fn engine_state(state: EngineState) -> (proto::EngineState, contr_api::common::v1::ErrorReason) {
    let unspecified = contr_api::common::v1::ErrorReason::Unspecified;
    match state {
        EngineState::Uninitialized => (proto::EngineState::Uninitialized, unspecified),
        EngineState::Provisioning => (proto::EngineState::Provisioning, unspecified),
        EngineState::Stopped => (proto::EngineState::Stopped, unspecified),
        EngineState::Starting => (proto::EngineState::Starting, unspecified),
        EngineState::Running => (proto::EngineState::Running, unspecified),
        EngineState::Stopping => (proto::EngineState::Stopping, unspecified),
        EngineState::Updating => (proto::EngineState::Updating, unspecified),
        EngineState::Resetting => (proto::EngineState::Resetting, unspecified),
        EngineState::Failed(reason) => (proto::EngineState::Failed, status::proto_reason(reason)),
    }
}
