use async_trait::async_trait;
use contr_testkit::{FakeEngine, Fixtures};
use tokio::sync::Mutex;

use crate::{
    Backend, BackendError, BackendKind, Capability, EngineConnector, Probe, Progress, ProgressSink, ResetScope,
};

/// Backend whose engine is a [`FakeEngine`]; every start serves a fresh copy of the fixtures.
pub struct FakeBackend {
    fixtures: Fixtures,
    engine: Mutex<Option<FakeEngine>>,
}

impl FakeBackend {
    /// Backend that serves `fixtures` once started.
    pub fn new(fixtures: Fixtures) -> Self {
        Self {
            fixtures,
            engine: Mutex::new(None),
        }
    }
}

#[async_trait]
impl Backend for FakeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Fake
    }

    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::Reset, Capability::Compose]
    }

    async fn probe(&self) -> Result<Probe, BackendError> {
        Ok(Probe::default())
    }

    async fn provision(&self, _progress: &dyn ProgressSink) -> Result<(), BackendError> {
        Ok(())
    }

    async fn running(&self) -> Result<Option<EngineConnector>, BackendError> {
        Ok(self.engine.lock().await.as_ref().map(connector))
    }

    async fn start(&self, progress: &dyn ProgressSink) -> Result<EngineConnector, BackendError> {
        let mut engine = self.engine.lock().await;
        if let Some(engine) = engine.as_ref() {
            return Ok(connector(engine));
        }

        progress.report(Progress {
            message: "Starting the fake engine".to_owned(),
            fraction: None,
        });
        let started = FakeEngine::start(self.fixtures.clone())?;
        let connector = connector(&started);
        *engine = Some(started);

        Ok(connector)
    }

    async fn stop(&self, _progress: &dyn ProgressSink) -> Result<(), BackendError> {
        self.engine.lock().await.take();
        Ok(())
    }

    async fn reset(&self, _scope: ResetScope, _progress: &dyn ProgressSink) -> Result<(), BackendError> {
        self.engine.lock().await.take();
        Ok(())
    }
}

#[cfg(unix)]
fn connector(engine: &FakeEngine) -> EngineConnector {
    EngineConnector::UnixSocket(engine.socket_path().to_path_buf())
}

#[cfg(windows)]
fn connector(engine: &FakeEngine) -> EngineConnector {
    EngineConnector::NamedPipe(engine.pipe_name().to_owned())
}
