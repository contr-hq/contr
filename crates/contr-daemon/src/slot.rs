//! The engine client and cache that exist while the supervisor reports the engine running.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use contr_api::daemon::v1::EngineInfo;
use contr_backend::{EngineConnector, EngineState, EngineStatus, EngineSupervisor};
use contr_engine::{Engine, EngineCache, EngineError};
use contr_platform::docker_context;
use tokio::sync::watch;
use tonic::Status;

use crate::status;

const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const READY_WAIT: Duration = Duration::from_secs(10);

/// Docker CLI context the daemon keeps pointed at the running engine.
#[derive(Clone, Debug)]
pub(crate) struct DockerContext {
    pub(crate) config_dir: PathBuf,
    pub(crate) name: &'static str,
}

pub(crate) struct EngineHandle {
    pub(crate) engine: Engine,
    pub(crate) cache: EngineCache,
    pub(crate) info: EngineInfo,
}

impl EngineHandle {
    async fn connect(connector: &EngineConnector, context: Option<&DockerContext>) -> Result<Self, EngineError> {
        let engine = Engine::connect(connector).await?;
        let mut info = engine.info().await?;
        if let Some(context) = context {
            info.docker_context = point_context(context, engine.endpoint());
        }
        let cache = EngineCache::start(engine.clone()).await?;

        Ok(Self { engine, cache, info })
    }
}

/// Current engine handle; `None` whenever the engine is not running.
#[derive(Clone)]
pub(crate) struct EngineSlot {
    current: watch::Receiver<Option<Arc<EngineHandle>>>,
    status: watch::Receiver<EngineStatus>,
}

impl EngineSlot {
    pub(crate) fn spawn(supervisor: &EngineSupervisor, context: Option<DockerContext>) -> Self {
        let (sender, current) = watch::channel(None);
        tokio::spawn(follow(supervisor.subscribe(), sender, context));
        Self {
            current,
            status: supervisor.subscribe(),
        }
    }

    /// The handle; while the supervisor reports a running engine whose handle is still connecting, waits for it.
    pub(crate) async fn get(&self) -> Result<Arc<EngineHandle>, Status> {
        if let Some(handle) = self.current.borrow().clone() {
            return Ok(handle);
        }
        if self.status.borrow().state() != EngineState::Running {
            return Err(status::engine_not_running());
        }

        let mut current = self.current.clone();
        match tokio::time::timeout(READY_WAIT, current.wait_for(Option::is_some)).await {
            Ok(Ok(handle)) => handle.clone().ok_or_else(status::engine_not_running),
            _ => Err(status::engine_not_running()),
        }
    }

    pub(crate) fn info(&self) -> Option<EngineInfo> {
        self.current.borrow().as_ref().map(|handle| handle.info.clone())
    }
}

async fn follow(
    mut status: watch::Receiver<EngineStatus>,
    sender: watch::Sender<Option<Arc<EngineHandle>>>,
    context: Option<DockerContext>,
) {
    loop {
        let connector = status.borrow_and_update().connector().cloned();
        let handle = match connector {
            Some(connector) => match EngineHandle::connect(&connector, context.as_ref()).await {
                Ok(handle) => Some(Arc::new(handle)),
                Err(error) => {
                    tracing::warn!(%error, "cannot connect to the running engine; retrying");
                    replace(&sender, None);
                    tokio::select! {
                        changed = status.changed() => if changed.is_err() { return },
                        () = tokio::time::sleep(RECONNECT_DELAY) => {}
                    }
                    continue;
                }
            },
            None => None,
        };

        replace(&sender, handle);
        if status.changed().await.is_err() {
            return;
        }
    }
}

fn replace(sender: &watch::Sender<Option<Arc<EngineHandle>>>, handle: Option<Arc<EngineHandle>>) {
    if let Some(previous) = sender.send_replace(handle) {
        previous.cache.close();
    }
}

/// Returns the context name when it targets `host`, or an empty string when it could not be written.
fn point_context(context: &DockerContext, host: &str) -> String {
    let description = format!("{} engine", contr_core::product::DISPLAY_NAME);
    match docker_context::ensure(&context.config_dir, context.name, &description, host) {
        Ok(outcome) => {
            tracing::info!(
                ?outcome,
                context = context.name,
                host,
                "docker context points at the engine"
            );
            context.name.to_owned()
        }
        Err(error) => {
            tracing::warn!(%error, context = context.name, "cannot write the docker context");
            String::new()
        }
    }
}
