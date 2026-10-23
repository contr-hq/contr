use std::sync::Arc;

use contr_api::operations::v1 as proto;
use contr_api::operations::v1::follow_operation_response::Event;
use contr_api::operations::v1::operation_request::Kind;
use contr_api::operations::v1::operations_service_server::OperationsService;
use futures_util::StreamExt;
use tonic::{Request, Response, Status};

use super::compose::ComposeApi;
use super::{ResponseStream, Shared, diagnostics, engine, images, kubernetes};
use crate::operations::Followed;

pub(crate) struct OperationsApi {
    shared: Arc<Shared>,
    compose: ComposeApi,
}

impl OperationsApi {
    pub(crate) fn new(shared: Arc<Shared>, compose: ComposeApi) -> Self {
        Self { shared, compose }
    }
}

#[tonic::async_trait]
impl OperationsService for OperationsApi {
    type WatchOperationsStream = ResponseStream<proto::WatchOperationsResponse>;
    type FollowOperationStream = ResponseStream<proto::FollowOperationResponse>;

    async fn start_operation(
        &self,
        request: Request<proto::StartOperationRequest>,
    ) -> Result<Response<proto::StartOperationResponse>, Status> {
        let kind = request
            .into_inner()
            .request
            .and_then(|request| request.kind)
            .ok_or_else(|| Status::invalid_argument("name the operation to start"))?;
        let started = match kind {
            Kind::PullImage(request) => images::start_pull(&self.shared, request).await?,
            Kind::PushImage(request) => images::start_push(&self.shared, request).await?,
            Kind::ComposeAction(request) => self.compose.start_action(request).await?,
            Kind::ImportImages(request) => kubernetes::start_import(&self.shared, request).await?,
            Kind::CreateBundle(_) => diagnostics::start_bundle(&self.shared)?,
            Kind::RunRemediation(request) => diagnostics::start_remediation(&self.shared, request)?,
            kind @ (Kind::StartEngine(_) | Kind::StopEngine(_) | Kind::RestartEngine(_) | Kind::ResetEngine(_)) => {
                engine::start_engine(&self.shared, kind)?
            }
        };

        Ok(Response::new(proto::StartOperationResponse {
            operation: Some(started.operation),
        }))
    }

    async fn watch_operations(
        &self,
        _: Request<proto::WatchOperationsRequest>,
    ) -> Result<Response<Self::WatchOperationsStream>, Status> {
        let changes = self
            .shared
            .operations
            .watch()
            .map(|event| Ok(proto::WatchOperationsResponse { event: Some(event) }));

        Ok(Response::new(self.shared.until_stopping(changes.boxed())))
    }

    async fn follow_operation(
        &self,
        request: Request<proto::FollowOperationRequest>,
    ) -> Result<Response<Self::FollowOperationStream>, Status> {
        let events = self
            .shared
            .operations
            .follow(&request.into_inner().id)?
            .map(|followed| {
                Ok(proto::FollowOperationResponse {
                    event: Some(match followed {
                        Followed::Event(event) => event,
                        Followed::Finished(operation) => Event::Finished(operation),
                    }),
                })
            });

        Ok(Response::new(self.shared.until_stopping(events.boxed())))
    }

    async fn cancel_operation(
        &self,
        request: Request<proto::CancelOperationRequest>,
    ) -> Result<Response<proto::CancelOperationResponse>, Status> {
        self.shared.operations.cancel(&request.into_inner().id)?;

        Ok(Response::new(proto::CancelOperationResponse {}))
    }
}
