use std::sync::Arc;

use contr_api::kubernetes::v1 as proto;
use contr_api::kubernetes::v1::kubernetes_service_server::KubernetesService;
use contr_core::product;
use contr_kube::{cluster, kubeconfig};
use futures_util::StreamExt;
use tokio_stream::wrappers::WatchStream;
use tonic::{Request, Response, Status};

use super::{ResponseStream, Shared};

pub(crate) struct KubernetesApi(pub(crate) Arc<Shared>);

#[tonic::async_trait]
impl KubernetesService for KubernetesApi {
    type WatchClusterStream = ResponseStream<proto::WatchClusterResponse>;
    type ImportImagesStream = ResponseStream<proto::ImportImagesResponse>;

    async fn get_cluster(
        &self,
        _: Request<proto::GetClusterRequest>,
    ) -> Result<Response<proto::GetClusterResponse>, Status> {
        Ok(Response::new(proto::GetClusterResponse {
            cluster: Some(self.0.kubernetes.cluster()),
        }))
    }

    async fn watch_cluster(
        &self,
        _: Request<proto::WatchClusterRequest>,
    ) -> Result<Response<Self::WatchClusterStream>, Status> {
        let states = WatchStream::new(self.0.kubernetes.subscribe())
            .map(|cluster| Ok(proto::WatchClusterResponse { cluster: Some(cluster) }));

        Ok(Response::new(states.boxed()))
    }

    async fn reset_cluster(
        &self,
        _: Request<proto::ResetClusterRequest>,
    ) -> Result<Response<proto::ResetClusterResponse>, Status> {
        self.0.slot.get().await?;
        self.0.kubernetes.request_reset();

        Ok(Response::new(proto::ResetClusterResponse {}))
    }

    async fn import_images(
        &self,
        request: Request<proto::ImportImagesRequest>,
    ) -> Result<Response<Self::ImportImagesStream>, Status> {
        let images = request.into_inner().images;
        if images.is_empty() {
            return Err(Status::invalid_argument("name at least one image"));
        }
        if self.0.kubernetes.cluster().state() != proto::ClusterState::Ready {
            return Err(Status::failed_precondition("the Kubernetes cluster is not ready"));
        }
        let engine = self.0.slot.get().await?;

        let outcomes = async_stream::stream! {
            for image in images {
                let error = match cluster::import(&engine.engine, &image).await {
                    Ok(()) => String::new(),
                    Err(error) => error.to_string(),
                };
                yield Ok(proto::ImportImagesResponse { image, error });
            }
        };

        Ok(Response::new(outcomes.boxed()))
    }

    async fn set_current_context(
        &self,
        request: Request<proto::SetCurrentContextRequest>,
    ) -> Result<Response<proto::SetCurrentContextResponse>, Status> {
        let current = request.into_inner().current;
        let path = self
            .0
            .kubernetes
            .kubeconfig
            .clone()
            .ok_or_else(|| Status::failed_precondition("Contr cannot find your kubeconfig"))?;
        let unusable = |error: kubeconfig::KubeconfigError| Status::internal(error.to_string());
        let now = kubeconfig::current_context(&path).map_err(unusable)?;

        if current {
            if now.as_deref() != Some(product::KUBE_CONTEXT) {
                kubeconfig::set_current_context(&path, Some(product::KUBE_CONTEXT)).map_err(unusable)?;
                self.0
                    .settings
                    .update(None, self.0.backend.as_ref(), |stored| {
                        stored.previous_kube_context = now;
                        Ok(())
                    })
                    .await?;
            }
        } else if now.as_deref() == Some(product::KUBE_CONTEXT) {
            let (previous, _) = self
                .0
                .settings
                .update(None, self.0.backend.as_ref(), |stored| {
                    Ok(stored.previous_kube_context.take())
                })
                .await?;
            kubeconfig::set_current_context(&path, previous.as_deref()).map_err(unusable)?;
        }
        self.0.kubernetes.republish();

        Ok(Response::new(proto::SetCurrentContextResponse {
            cluster: Some(self.0.kubernetes.cluster()),
        }))
    }
}
