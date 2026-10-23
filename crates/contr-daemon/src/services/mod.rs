//! Thin gRPC service implementations over the supervisor and the engine slot.

mod compose;
mod containers;
mod daemon;
mod diagnostics;
mod engine;
mod images;
mod integration;
mod kubernetes;
mod networks;
mod operations;
mod settings;
mod updates;
mod volumes;

use std::sync::Arc;
use std::time::SystemTime;

use contr_api::operations::v1::OperationState;
use contr_api::operations::v1::follow_operation_response::Event;
use contr_backend::{Backend, BackendKind, Capability, EngineSupervisor};
use contr_diagnostics::LogRing;
use futures_util::stream::{self, BoxStream};
use futures_util::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;
use tonic::Status;

pub(crate) use compose::ComposeApi;
pub(crate) use containers::ContainersApi;
pub(crate) use daemon::DaemonApi;
pub(crate) use diagnostics::DiagnosticsApi;
pub(crate) use engine::EngineApi;
pub(crate) use images::ImagesApi;
pub(crate) use integration::{CliLocations, IntegrationApi, WslAccess};
pub(crate) use kubernetes::KubernetesApi;
pub(crate) use networks::NetworksApi;
pub(crate) use operations::OperationsApi;
pub(crate) use settings::SettingsApi;
pub(crate) use updates::UpdatesApi;
pub(crate) use volumes::VolumesApi;

use crate::kubernetes::KubernetesHub;
use crate::operations::{Followed, Operations, Started};
use crate::settings::SettingsHub;
use crate::slot::{DockerContext, EngineSlot};
use crate::status;
use crate::updates::UpdatesHub;

pub(crate) type ResponseStream<T> = BoxStream<'static, Result<T, Status>>;

/// State every service reads.
pub(crate) struct Shared {
    pub(crate) supervisor: EngineSupervisor,
    pub(crate) slot: EngineSlot,
    pub(crate) backend_kind: BackendKind,
    pub(crate) capabilities: &'static [Capability],
    pub(crate) started_at: SystemTime,
    /// Bundled and installed Docker CLI tools; `None` where the daemon bundles none.
    pub(crate) cli: Option<CliLocations>,
    /// Engine access from the user's WSL distributions; `None` where the engine does not run in WSL.
    pub(crate) wsl: Option<WslAccess>,
    pub(crate) backend: Arc<dyn Backend>,
    pub(crate) settings: Arc<SettingsHub>,
    /// Docker CLI context the daemon keeps pointed at the engine; `None` when it manages none.
    pub(crate) docker_context: Option<DockerContext>,
    pub(crate) kubernetes: Arc<KubernetesHub>,
    pub(crate) updates: Arc<UpdatesHub>,
    pub(crate) log: Option<LogRing>,
    pub(crate) operations: Arc<Operations>,
    /// Cancelled once the daemon begins to stop.
    pub(crate) stopping: CancellationToken,
}

impl Shared {
    /// Ends `stream` once the daemon begins to stop, since graceful shutdown waits for every open stream.
    pub(crate) fn until_stopping<T: Send + 'static>(&self, stream: ResponseStream<T>) -> ResponseStream<T> {
        stream.take_until(self.stopping.clone().cancelled_owned()).boxed()
    }
}

/// A watch stream ends only when its engine cache closes, which clients see as `ENGINE_NOT_RUNNING`.
fn watch<C, T>(changes: C, convert: fn(C::Item) -> T) -> ResponseStream<T>
where
    C: Stream + Send + 'static,
    T: Send + 'static,
{
    changes
        .map(move |change| Ok(convert(change)))
        .chain(stream::once(async { Err(status::engine_not_running()) }))
        .boxed()
}

/// The stream of an RPC that predates operations for `started`: its events as `convert` turns them into responses,
/// then the operation's failure as the RPC's status unless an event `reports` it. With `cancel_on_drop`, dropping the
/// stream cancels an operation this call started.
fn follow_started<T: Send + 'static>(
    shared: &Shared,
    started: &Started,
    cancel_on_drop: bool,
    convert: fn(Event) -> Option<T>,
    reports: fn(&Event) -> bool,
) -> Result<ResponseStream<T>, Status> {
    let id = started.operation.id.clone();
    let followed = shared.operations.follow(&id)?;
    let guard = (cancel_on_drop && !started.joined).then(|| CancelOnDrop {
        operations: Arc::clone(&shared.operations),
        id,
    });
    let mut reported = false;

    Ok(followed
        .filter_map(move |item| {
            let _ = &guard;
            let response = match item {
                Followed::Event(event) => {
                    reported |= reports(&event);
                    convert(event).map(Ok)
                }
                Followed::Finished(operation) => match operation.state() {
                    OperationState::Failed if !reported => operation
                        .error
                        .as_ref()
                        .map(|error| Err(status::from_operation_error(error))),
                    OperationState::Cancelled => Some(Err(Status::cancelled("the operation was cancelled"))),
                    _ => None,
                },
            };
            async move { response }
        })
        .boxed())
}

struct CancelOnDrop {
    operations: Arc<Operations>,
    id: String,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let _ = self.operations.cancel(&self.id);
    }
}
