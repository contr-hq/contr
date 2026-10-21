use std::sync::Arc;

use contr_api::updates::v1 as proto;
use contr_api::updates::v1::updates_service_server::UpdatesService;
use tonic::{Request, Response, Status};

use super::Shared;

pub(crate) struct UpdatesApi(pub(crate) Arc<Shared>);

#[tonic::async_trait]
impl UpdatesService for UpdatesApi {
    async fn check_for_updates(
        &self,
        _: Request<proto::CheckForUpdatesRequest>,
    ) -> Result<Response<proto::CheckForUpdatesResponse>, Status> {
        let status = self.0.updates.check().await?;
        Ok(Response::new(proto::CheckForUpdatesResponse { status: Some(status) }))
    }

    async fn get_update_status(
        &self,
        _: Request<proto::GetUpdateStatusRequest>,
    ) -> Result<Response<proto::GetUpdateStatusResponse>, Status> {
        Ok(Response::new(proto::GetUpdateStatusResponse {
            status: Some(self.0.updates.status()),
        }))
    }
}
