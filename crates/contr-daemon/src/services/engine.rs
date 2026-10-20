use std::sync::Arc;

use contr_api::common::v1::Progress;
use contr_api::engine::v1 as proto;
use contr_api::engine::v1::engine_service_server::EngineService;
use contr_api::engine::v1::{reset_response, restart_response, start_response, stop_response};
use contr_backend::{EngineState, OperationEvent, ResetScope};
use futures_util::stream::BoxStream;
use futures_util::{Stream, StreamExt};
use tokio_stream::wrappers::WatchStream;
use tonic::{Request, Response, Status};

use super::{ResponseStream, Shared};
use crate::status;

pub(crate) struct EngineApi(pub(crate) Arc<Shared>);

#[tonic::async_trait]
impl EngineService for EngineApi {
    type WatchStateStream = ResponseStream<proto::WatchStateResponse>;
    type StartStream = ResponseStream<proto::StartResponse>;
    type StopStream = ResponseStream<proto::StopResponse>;
    type RestartStream = ResponseStream<proto::RestartResponse>;
    type ResetStream = ResponseStream<proto::ResetResponse>;

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

        Ok(Response::new(states.boxed()))
    }

    async fn start(&self, _: Request<proto::StartRequest>) -> Result<Response<Self::StartStream>, Status> {
        let events = self.until_ready(self.0.supervisor.start().await);
        Ok(Response::new(operation(
            events,
            |progress| proto::StartResponse {
                event: Some(start_response::Event::Progress(progress)),
            },
            |state| proto::StartResponse {
                event: Some(start_response::Event::State(state.into())),
            },
        )))
    }

    async fn stop(&self, _: Request<proto::StopRequest>) -> Result<Response<Self::StopStream>, Status> {
        let events = self.0.supervisor.stop().await.boxed();
        Ok(Response::new(operation(
            events,
            |progress| proto::StopResponse {
                event: Some(stop_response::Event::Progress(progress)),
            },
            |state| proto::StopResponse {
                event: Some(stop_response::Event::State(state.into())),
            },
        )))
    }

    async fn restart(&self, _: Request<proto::RestartRequest>) -> Result<Response<Self::RestartStream>, Status> {
        let events = self.until_ready(self.0.supervisor.restart().await);
        Ok(Response::new(operation(
            events,
            |progress| proto::RestartResponse {
                event: Some(restart_response::Event::Progress(progress)),
            },
            |state| proto::RestartResponse {
                event: Some(restart_response::Event::State(state.into())),
            },
        )))
    }

    async fn reset(&self, request: Request<proto::ResetRequest>) -> Result<Response<Self::ResetStream>, Status> {
        let scope = match request.into_inner().scope() {
            proto::ResetScope::Runtime => ResetScope::Runtime,
            proto::ResetScope::Factory => ResetScope::Factory,
            proto::ResetScope::Unspecified => return Err(Status::invalid_argument("a reset scope is required")),
        };
        let events = self.0.supervisor.reset(scope).await.boxed();

        Ok(Response::new(operation(
            events,
            |progress| proto::ResetResponse {
                event: Some(reset_response::Event::Progress(progress)),
            },
            |state| proto::ResetResponse {
                event: Some(reset_response::Event::State(state.into())),
            },
        )))
    }
}

impl EngineApi {
    /// Holds back a successful result until the engine handle is ready, so calls made after it find the engine.
    fn until_ready(
        &self,
        events: impl Stream<Item = OperationEvent> + Send + 'static,
    ) -> BoxStream<'static, OperationEvent> {
        let slot = self.0.slot.clone();
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
}

fn operation<T: Send + 'static>(
    events: BoxStream<'static, OperationEvent>,
    progress: fn(Progress) -> T,
    finished: fn(proto::EngineState) -> T,
) -> ResponseStream<T> {
    events
        .map(move |event| match event {
            OperationEvent::Progress(step) => Ok(progress(Progress {
                message: step.message,
                fraction: step.fraction,
            })),
            OperationEvent::Finished(Ok(state)) => Ok(finished(engine_state(state).0)),
            OperationEvent::Finished(Err(error)) => Err(status::from_operation(&error)),
        })
        .boxed()
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
