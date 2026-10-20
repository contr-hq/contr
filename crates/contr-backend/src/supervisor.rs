//! Actor that owns a [`Backend`] and runs every lifecycle operation through one state machine.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use contr_core::{ErrorReason, Remediation};
use tokio::sync::{mpsc, watch};
use tokio::time::{self, Instant, MissedTickBehavior};
use tokio_stream::wrappers::ReceiverStream;

use crate::{Backend, BackendError, Capability, EngineConnector, EngineState, Progress, ProgressSink, ResetScope};

const COMMAND_QUEUE: usize = 16;
const EVENT_QUEUE: usize = 64;
const READY_POLL_INTERVAL: Duration = Duration::from_millis(250);
const MISSED_PINGS_BEFORE_FAILURE: u32 = 2;
const MAX_RECOVERY_ATTEMPTS: u32 = 5;
const FIRST_RECOVERY_DELAY: Duration = Duration::from_secs(5);
const MAX_RECOVERY_DELAY: Duration = Duration::from_secs(120);

/// Checks whether a Docker API answers; the daemon implements it with its engine client.
#[async_trait]
pub trait EngineHealth: Send + Sync + 'static {
    /// Returns `true` when the API at `connector` answers a ping.
    async fn ping(&self, connector: &EngineConnector) -> bool;
}

/// Timing of an [`EngineSupervisor`].
#[derive(Clone, Copy, Debug)]
pub struct SupervisorConfig {
    /// Interval between health checks of a running engine, and between checks for an engine started elsewhere.
    pub poll_interval: Duration,
    /// Time a started engine has to answer before the start fails.
    pub ready_timeout: Duration,
    /// Restarts an engine that stops answering without a stop request, with growing delays, at most five times.
    /// Only for backends whose engine nothing but the daemon starts and stops.
    pub recover: bool,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            ready_timeout: Duration::from_secs(60),
            recover: false,
        }
    }
}

/// Engine state plus, while running, where its Docker API listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineStatus {
    state: EngineState,
    connector: Option<EngineConnector>,
}

impl EngineStatus {
    fn running(connector: EngineConnector) -> Self {
        Self {
            state: EngineState::Running,
            connector: Some(connector),
        }
    }

    fn not_running(state: EngineState) -> Self {
        debug_assert_ne!(state, EngineState::Running);
        Self { state, connector: None }
    }

    /// Current lifecycle state.
    pub fn state(&self) -> EngineState {
        self.state
    }

    /// Docker API endpoint; `Some` exactly while the state is [`EngineState::Running`].
    pub fn connector(&self) -> Option<&EngineConnector> {
        self.connector.as_ref()
    }
}

/// Failure of a lifecycle operation, in the shape clients receive it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct OperationError {
    /// Machine-readable cause.
    pub reason: ErrorReason,
    /// Developer-facing description.
    pub message: String,
    /// Actions that resolve the failure, in the order clients should offer them.
    pub remediations: Vec<Remediation>,
}

impl OperationError {
    fn new(reason: ErrorReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
            remediations: Vec::new(),
        }
    }
}

impl From<BackendError> for OperationError {
    fn from(error: BackendError) -> Self {
        match error {
            BackendError::Blocked(blocker) => Self {
                reason: blocker.reason,
                message: format!("the engine cannot run: {}", blocker.reason),
                remediations: blocker.remediations,
            },
            BackendError::Io(error) => Self::new(ErrorReason::EngineNotRunning, error.to_string()),
        }
    }
}

/// Event of a lifecycle operation stream.
#[derive(Clone, Debug, PartialEq)]
pub enum OperationEvent {
    /// Step report; dropped when the receiver falls behind.
    Progress(Progress),
    /// Last event: the resulting state or the failure.
    Finished(Result<EngineState, OperationError>),
}

#[derive(Clone, Copy, Debug)]
enum Operation {
    Start,
    Stop,
    Restart,
    Reset(ResetScope),
}

struct Command {
    operation: Operation,
    events: mpsc::Sender<OperationEvent>,
}

/// Handle to the actor that runs a [`Backend`]'s lifecycle; operations queue and run one at a time, so every state
/// change comes from a single task.
#[derive(Clone)]
pub struct EngineSupervisor {
    commands: mpsc::Sender<Command>,
    status: watch::Receiver<EngineStatus>,
}

impl EngineSupervisor {
    /// Detects whether the engine already runs, then spawns the actor on the current Tokio runtime. The actor stops
    /// once every handle is dropped.
    pub async fn spawn(backend: Arc<dyn Backend>, health: Arc<dyn EngineHealth>, config: SupervisorConfig) -> Self {
        let (status_sender, status) = watch::channel(EngineStatus::not_running(EngineState::Stopped));
        let actor = Actor {
            backend,
            health,
            config,
            status: status_sender,
            missed_pings: 0,
            recovery: None,
        };
        let initial = actor.detect().await;
        actor.publish(initial);

        let (commands, receiver) = mpsc::channel(COMMAND_QUEUE);
        tokio::spawn(actor.run(receiver));

        Self { commands, status }
    }

    /// Current status.
    pub fn status(&self) -> EngineStatus {
        self.status.borrow().clone()
    }

    /// Receiver that observes every status change.
    pub fn subscribe(&self) -> watch::Receiver<EngineStatus> {
        self.status.clone()
    }

    /// Provisions the engine when needed and starts it.
    pub async fn start(&self) -> ReceiverStream<OperationEvent> {
        self.submit(Operation::Start).await
    }

    /// Stops the engine and its containers.
    pub async fn stop(&self) -> ReceiverStream<OperationEvent> {
        self.submit(Operation::Stop).await
    }

    /// Stops the engine when it runs, then starts it.
    pub async fn restart(&self) -> ReceiverStream<OperationEvent> {
        self.submit(Operation::Restart).await
    }

    /// Stops the engine, then restores it to a clean state; fails before stopping anything when the backend lacks
    /// [`Capability::Reset`].
    pub async fn reset(&self, scope: ResetScope) -> ReceiverStream<OperationEvent> {
        self.submit(Operation::Reset(scope)).await
    }

    async fn submit(&self, operation: Operation) -> ReceiverStream<OperationEvent> {
        let (events, receiver) = mpsc::channel(EVENT_QUEUE);
        let command = Command {
            operation,
            events: events.clone(),
        };

        if self.commands.send(command).await.is_err() {
            let error = OperationError::new(ErrorReason::EngineNotRunning, "the engine supervisor has stopped");
            // The receiver is held below, so this send cannot fail.
            let _ = events.send(OperationEvent::Finished(Err(error))).await;
        }

        ReceiverStream::new(receiver)
    }
}

struct ChannelSink(mpsc::Sender<OperationEvent>);

impl ProgressSink for ChannelSink {
    fn report(&self, progress: Progress) {
        let _ = self.0.try_send(OperationEvent::Progress(progress));
    }
}

struct LogSink;

impl ProgressSink for LogSink {
    fn report(&self, progress: Progress) {
        tracing::debug!(message = progress.message, "recovery progress");
    }
}

#[derive(Clone, Copy, Debug)]
struct Recovery {
    attempts: u32,
    next_attempt: Instant,
}

struct Actor {
    backend: Arc<dyn Backend>,
    health: Arc<dyn EngineHealth>,
    config: SupervisorConfig,
    status: watch::Sender<EngineStatus>,
    missed_pings: u32,
    recovery: Option<Recovery>,
}

impl Actor {
    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        let mut poll = time::interval_at(Instant::now() + self.config.poll_interval, self.config.poll_interval);
        poll.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    self.execute(command).await;
                }
                _ = poll.tick() => self.check_health().await,
            }
        }
    }

    async fn execute(&mut self, Command { operation, events }: Command) {
        self.recovery = None;
        let sink = ChannelSink(events.clone());
        let result = match operation {
            Operation::Start => self.start(&sink).await,
            Operation::Stop => self.stop(&sink).await,
            Operation::Restart => match self.stop(&sink).await {
                Ok(_) => self.start(&sink).await,
                Err(error) => Err(error),
            },
            Operation::Reset(scope) => self.reset(scope, &sink).await,
        };

        // The caller may have dropped the stream; the operation still completed.
        let _ = events.send(OperationEvent::Finished(result)).await;
    }

    async fn start(&mut self, sink: &dyn ProgressSink) -> Result<EngineState, OperationError> {
        if self.state() == EngineState::Running {
            return Ok(EngineState::Running);
        }

        match self.try_start(sink).await {
            Ok(connector) => {
                self.publish(EngineStatus::running(connector));
                Ok(EngineState::Running)
            }
            Err(error) => {
                self.publish(EngineStatus::not_running(EngineState::Failed(error.reason)));
                Err(error)
            }
        }
    }

    async fn try_start(&mut self, sink: &dyn ProgressSink) -> Result<EngineConnector, OperationError> {
        let probe = self.backend.probe().await?;
        if let Some(blocker) = probe.blockers.into_iter().next() {
            return Err(BackendError::Blocked(blocker).into());
        }

        if self.state() == EngineState::Uninitialized {
            self.publish(EngineStatus::not_running(EngineState::Provisioning));
        }
        self.backend.provision(sink).await?;

        self.publish(EngineStatus::not_running(EngineState::Starting));
        let connector = self.backend.start(sink).await?;

        sink.report(Progress {
            message: "Waiting for the engine API".to_owned(),
            fraction: None,
        });
        self.wait_until_ready(&connector).await?;

        Ok(connector)
    }

    async fn wait_until_ready(&self, connector: &EngineConnector) -> Result<(), OperationError> {
        let deadline = Instant::now() + self.config.ready_timeout;

        while !self.health.ping(connector).await {
            if Instant::now() >= deadline {
                let message = format!(
                    "the engine API did not answer within {} seconds",
                    self.config.ready_timeout.as_secs()
                );
                return Err(OperationError::new(ErrorReason::EngineNotRunning, message));
            }
            time::sleep(READY_POLL_INTERVAL).await;
        }

        Ok(())
    }

    async fn stop(&mut self, sink: &dyn ProgressSink) -> Result<EngineState, OperationError> {
        let state = self.state();
        if matches!(state, EngineState::Stopped | EngineState::Uninitialized) {
            return Ok(state);
        }

        self.publish(EngineStatus::not_running(EngineState::Stopping));
        match self.backend.stop(sink).await {
            Ok(()) => {
                self.publish(EngineStatus::not_running(EngineState::Stopped));
                Ok(EngineState::Stopped)
            }
            Err(error) => {
                let status = self.detect().await;
                self.publish(status);
                Err(error.into())
            }
        }
    }

    async fn reset(&mut self, scope: ResetScope, sink: &dyn ProgressSink) -> Result<EngineState, OperationError> {
        if !self.backend.capabilities().contains(&Capability::Reset) {
            return Err(OperationError::new(
                ErrorReason::FeatureUnavailable,
                "this engine is not managed by Contr, so Contr cannot reset it",
            ));
        }
        self.stop(sink).await?;

        self.publish(EngineStatus::not_running(EngineState::Resetting));
        match self.backend.reset(scope, sink).await {
            Ok(()) => {
                let state = match scope {
                    ResetScope::Runtime => EngineState::Stopped,
                    ResetScope::Factory => EngineState::Uninitialized,
                };
                self.publish(EngineStatus::not_running(state));
                Ok(state)
            }
            Err(error) => {
                let status = self.detect().await;
                self.publish(status);
                Err(error.into())
            }
        }
    }

    async fn check_health(&mut self) {
        if let Some(recovery) = self.recovery {
            if Instant::now() >= recovery.next_attempt {
                self.recover().await;
            }
            return;
        }
        let connector = self.status.borrow().connector().cloned();

        let Some(connector) = connector else {
            if let Some(connector) = self.detect_running().await {
                self.publish(EngineStatus::running(connector));
            }
            return;
        };

        if self.health.ping(&connector).await {
            self.missed_pings = 0;
            return;
        }

        self.missed_pings += 1;
        if self.missed_pings >= MISSED_PINGS_BEFORE_FAILURE {
            self.missed_pings = 0;
            if let Some(connector) = self.detect_running().await {
                self.publish(EngineStatus::running(connector));
                return;
            }
            self.publish(EngineStatus::not_running(EngineState::Failed(
                ErrorReason::EngineNotRunning,
            )));
            if self.config.recover {
                self.recovery = Some(Recovery {
                    attempts: 0,
                    next_attempt: Instant::now(),
                });
                self.recover().await;
            }
        }
    }

    async fn recover(&mut self) {
        let Some(mut recovery) = self.recovery.take() else {
            return;
        };
        recovery.attempts += 1;
        tracing::warn!(
            attempt = recovery.attempts,
            "restarting the engine after it stopped answering"
        );

        match self.start(&LogSink).await {
            Ok(_) => tracing::info!("engine recovered"),
            Err(error) if recovery.attempts < MAX_RECOVERY_ATTEMPTS => {
                let delay = recovery_delay(recovery.attempts);
                tracing::warn!(%error, ?delay, "engine recovery failed; retrying");
                recovery.next_attempt = Instant::now() + delay;
                self.recovery = Some(recovery);
            }
            Err(error) => tracing::error!(%error, "engine recovery failed; giving up"),
        }
    }

    async fn detect(&self) -> EngineStatus {
        if let Some(connector) = self.detect_running().await {
            return EngineStatus::running(connector);
        }

        let state = match self.backend.probe().await {
            Ok(probe) => probe
                .blockers
                .first()
                .map_or(EngineState::Stopped, |blocker| EngineState::Failed(blocker.reason)),
            Err(error) => EngineState::Failed(OperationError::from(error).reason),
        };
        EngineStatus::not_running(state)
    }

    async fn detect_running(&self) -> Option<EngineConnector> {
        match self.backend.running().await {
            Ok(Some(connector)) if self.health.ping(&connector).await => Some(connector),
            Ok(_) => None,
            Err(error) => {
                tracing::debug!(%error, "engine detection failed");
                None
            }
        }
    }

    fn state(&self) -> EngineState {
        self.status.borrow().state
    }

    fn publish(&self, status: EngineStatus) {
        self.status.send_if_modified(|current| {
            if *current == status {
                return false;
            }
            tracing::info!(state = ?status.state, "engine state changed");
            *current = status;
            true
        });
    }
}

fn recovery_delay(attempts: u32) -> Duration {
    FIRST_RECOVERY_DELAY
        .saturating_mul(2u32.saturating_pow(attempts.saturating_sub(1)))
        .min(MAX_RECOVERY_DELAY)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use futures_util::StreamExt;

    use super::*;
    use crate::{BackendKind, Blocker, Probe};

    #[derive(Default)]
    struct ScriptedBackend {
        engine_up: AtomicBool,
        start_brings_engine_up: AtomicBool,
        blocked: AtomicBool,
        resettable: AtomicBool,
        starts: AtomicU32,
    }

    fn connector() -> EngineConnector {
        EngineConnector::UnixSocket(PathBuf::from("/run/scripted.sock"))
    }

    #[async_trait]
    impl Backend for ScriptedBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Fake
        }

        fn capabilities(&self) -> &'static [Capability] {
            if self.resettable.load(Ordering::SeqCst) {
                &[Capability::Reset]
            } else {
                &[]
            }
        }

        async fn probe(&self) -> Result<Probe, BackendError> {
            let blockers = if self.blocked.load(Ordering::SeqCst) {
                vec![Blocker::new(ErrorReason::EngineNotInstalled)]
            } else {
                Vec::new()
            };
            Ok(Probe { blockers })
        }

        async fn provision(&self, _: &dyn ProgressSink) -> Result<(), BackendError> {
            Ok(())
        }

        async fn running(&self) -> Result<Option<EngineConnector>, BackendError> {
            Ok(self.engine_up.load(Ordering::SeqCst).then(connector))
        }

        async fn start(&self, _: &dyn ProgressSink) -> Result<EngineConnector, BackendError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            if self.start_brings_engine_up.load(Ordering::SeqCst) {
                self.engine_up.store(true, Ordering::SeqCst);
            }
            Ok(connector())
        }

        async fn stop(&self, _: &dyn ProgressSink) -> Result<(), BackendError> {
            self.engine_up.store(false, Ordering::SeqCst);
            Ok(())
        }

        async fn reset(&self, _: ResetScope, _: &dyn ProgressSink) -> Result<(), BackendError> {
            Ok(())
        }
    }

    struct BackendHealth(Arc<ScriptedBackend>);

    #[async_trait]
    impl EngineHealth for BackendHealth {
        async fn ping(&self, _: &EngineConnector) -> bool {
            self.0.engine_up.load(Ordering::SeqCst)
        }
    }

    async fn supervisor(backend: &Arc<ScriptedBackend>) -> EngineSupervisor {
        supervisor_with(backend, SupervisorConfig::default()).await
    }

    async fn supervisor_with(backend: &Arc<ScriptedBackend>, config: SupervisorConfig) -> EngineSupervisor {
        let health = Arc::new(BackendHealth(Arc::clone(backend)));
        EngineSupervisor::spawn(backend.clone(), health, config).await
    }

    fn recovering() -> SupervisorConfig {
        SupervisorConfig {
            recover: true,
            ..SupervisorConfig::default()
        }
    }

    /// Waits until the backend has been started `starts` times and the supervisor reports `state`; a watch keeps
    /// only the latest status, so states in between may never be observed.
    async fn wait_for(
        status: &mut watch::Receiver<EngineStatus>,
        backend: &ScriptedBackend,
        starts: u32,
        state: EngineState,
    ) {
        while backend.starts.load(Ordering::SeqCst) < starts || status.borrow_and_update().state() != state {
            status.changed().await.unwrap();
        }
    }

    fn working_backend() -> Arc<ScriptedBackend> {
        let backend = ScriptedBackend::default();
        backend.start_brings_engine_up.store(true, Ordering::SeqCst);
        Arc::new(backend)
    }

    async fn finished(events: ReceiverStream<OperationEvent>) -> Result<EngineState, OperationError> {
        let events: Vec<_> = events.collect().await;
        match events.last() {
            Some(OperationEvent::Finished(result)) => result.clone(),
            other => panic!("stream ended without a result: {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn detects_a_running_engine_at_spawn() {
        let backend = working_backend();
        backend.engine_up.store(true, Ordering::SeqCst);

        let status = supervisor(&backend).await.status();

        assert_eq!(status.state(), EngineState::Running);
        assert_eq!(status.connector(), Some(&connector()));
    }

    #[tokio::test(start_paused = true)]
    async fn start_then_stop() {
        let backend = working_backend();
        let supervisor = supervisor(&backend).await;
        assert_eq!(supervisor.status().state(), EngineState::Stopped);

        assert_eq!(finished(supervisor.start().await).await, Ok(EngineState::Running));
        assert_eq!(supervisor.status().connector(), Some(&connector()));

        assert_eq!(finished(supervisor.stop().await).await, Ok(EngineState::Stopped));
        assert_eq!(supervisor.status().connector(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn reset_stops_the_engine_and_reports_what_remains() {
        let backend = working_backend();
        backend.resettable.store(true, Ordering::SeqCst);
        let supervisor = supervisor(&backend).await;
        finished(supervisor.start().await).await.unwrap();

        let factory = finished(supervisor.reset(ResetScope::Factory).await).await;
        assert_eq!(factory, Ok(EngineState::Uninitialized));
        assert!(!backend.engine_up.load(Ordering::SeqCst));

        finished(supervisor.start().await).await.unwrap();
        let runtime = finished(supervisor.reset(ResetScope::Runtime).await).await;
        assert_eq!(runtime, Ok(EngineState::Stopped));
    }

    #[tokio::test(start_paused = true)]
    async fn reset_leaves_an_engine_it_does_not_own_running() {
        let backend = working_backend();
        let supervisor = supervisor(&backend).await;
        finished(supervisor.start().await).await.unwrap();

        let error = finished(supervisor.reset(ResetScope::Factory).await).await.unwrap_err();

        assert_eq!(error.reason, ErrorReason::FeatureUnavailable);
        assert_eq!(supervisor.status().state(), EngineState::Running);
    }

    #[tokio::test(start_paused = true)]
    async fn restart_ends_running() {
        let backend = working_backend();
        let supervisor = supervisor(&backend).await;

        assert_eq!(finished(supervisor.restart().await).await, Ok(EngineState::Running));
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_start_fails_with_the_blocker_reason() {
        let backend = working_backend();
        backend.blocked.store(true, Ordering::SeqCst);
        let supervisor = supervisor(&backend).await;
        assert_eq!(
            supervisor.status().state(),
            EngineState::Failed(ErrorReason::EngineNotInstalled)
        );

        let error = finished(supervisor.start().await).await.unwrap_err();

        assert_eq!(error.reason, ErrorReason::EngineNotInstalled);
        assert_eq!(
            supervisor.status().state(),
            EngineState::Failed(ErrorReason::EngineNotInstalled)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn start_fails_when_the_api_never_answers() {
        let backend = Arc::new(ScriptedBackend::default());
        let supervisor = supervisor(&backend).await;

        let error = finished(supervisor.start().await).await.unwrap_err();

        assert_eq!(error.reason, ErrorReason::EngineNotRunning);
        assert_eq!(
            supervisor.status().state(),
            EngineState::Failed(ErrorReason::EngineNotRunning)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn engine_that_disappears_fails_after_missed_pings() {
        let backend = working_backend();
        let supervisor = supervisor(&backend).await;
        finished(supervisor.start().await).await.unwrap();
        let mut status = supervisor.subscribe();
        status.borrow_and_update();

        backend.engine_up.store(false, Ordering::SeqCst);
        status.changed().await.unwrap();

        assert_eq!(
            status.borrow().state(),
            EngineState::Failed(ErrorReason::EngineNotRunning)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn adopts_an_engine_started_elsewhere() {
        let backend = working_backend();
        let supervisor = supervisor(&backend).await;
        let mut status = supervisor.subscribe();
        status.borrow_and_update();

        backend.engine_up.store(true, Ordering::SeqCst);
        status.changed().await.unwrap();

        assert_eq!(status.borrow().state(), EngineState::Running);
    }
    #[test]
    fn recovery_delays_double_up_to_the_cap() {
        let delays: Vec<_> = (1..=7).map(|attempt| recovery_delay(attempt).as_secs()).collect();

        assert_eq!(delays, [5, 10, 20, 40, 80, 120, 120]);
    }

    #[tokio::test(start_paused = true)]
    async fn lost_engine_is_restarted_when_recovering() {
        let backend = working_backend();
        let supervisor = supervisor_with(&backend, recovering()).await;
        finished(supervisor.start().await).await.unwrap();
        let mut status = supervisor.subscribe();

        backend.engine_up.store(false, Ordering::SeqCst);
        wait_for(&mut status, &backend, 2, EngineState::Running).await;

        assert_eq!(backend.starts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_gives_up_after_five_attempts() {
        let backend = working_backend();
        let supervisor = supervisor_with(&backend, recovering()).await;
        finished(supervisor.start().await).await.unwrap();

        backend.start_brings_engine_up.store(false, Ordering::SeqCst);
        backend.engine_up.store(false, Ordering::SeqCst);
        time::sleep(Duration::from_secs(3600)).await;

        assert_eq!(backend.starts.load(Ordering::SeqCst), 1 + MAX_RECOVERY_ATTEMPTS);
        assert_eq!(
            supervisor.status().state(),
            EngineState::Failed(ErrorReason::EngineNotRunning)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stop_request_ends_recovery() {
        let backend = working_backend();
        let supervisor = supervisor_with(&backend, recovering()).await;
        finished(supervisor.start().await).await.unwrap();
        backend.start_brings_engine_up.store(false, Ordering::SeqCst);
        backend.engine_up.store(false, Ordering::SeqCst);
        let mut status = supervisor.subscribe();
        wait_for(
            &mut status,
            &backend,
            2,
            EngineState::Failed(ErrorReason::EngineNotRunning),
        )
        .await;

        finished(supervisor.stop().await).await.unwrap();
        let starts = backend.starts.load(Ordering::SeqCst);
        time::sleep(Duration::from_secs(3600)).await;

        assert_eq!(backend.starts.load(Ordering::SeqCst), starts);
    }
}
