//! Thin gRPC service implementations over the supervisor and the engine slot.

mod containers;
mod daemon;
mod engine;
mod images;
mod networks;
mod volumes;

use std::time::SystemTime;

use contr_backend::{BackendKind, Capability, EngineSupervisor};
use futures_util::stream::{self, BoxStream};
use futures_util::{Stream, StreamExt};
use tonic::Status;

pub(crate) use containers::ContainersApi;
pub(crate) use daemon::DaemonApi;
pub(crate) use engine::EngineApi;
pub(crate) use images::ImagesApi;
pub(crate) use networks::NetworksApi;
pub(crate) use volumes::VolumesApi;

use crate::slot::EngineSlot;
use crate::status;

pub(crate) type ResponseStream<T> = BoxStream<'static, Result<T, Status>>;

/// State every service reads.
pub(crate) struct Shared {
    pub(crate) supervisor: EngineSupervisor,
    pub(crate) slot: EngineSlot,
    pub(crate) backend_kind: BackendKind,
    pub(crate) capabilities: &'static [Capability],
    pub(crate) started_at: SystemTime,
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
