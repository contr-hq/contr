//! Long-running work the daemon runs on its own tasks; see docs/adr/0010-daemon-owned-operations.md.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use contr_api::common::v1::Progress;
use contr_api::operations::v1::follow_operation_response::Event;
use contr_api::operations::v1::{
    Operation, OperationRequest, OperationSnapshot, OperationState, watch_operations_response,
};
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;
use tonic::Status;

use crate::status;

/// How long a finished operation stays listed.
const RETENTION: Duration = Duration::from_hours(1);
const MAX_FINISHED: usize = 50;
/// Compose output lines and pull status lines kept for replay.
const REPLAY_LINES: usize = 1_000;
/// Events a follower may fall behind by before it misses some.
const LIVE_EVENTS: usize = 1_024;
const WATCH_EVENTS: usize = 256;
/// Shortest gap between two summaries published for the steps of one operation.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Whether a new operation may start beside running ones.
pub(crate) enum Admission {
    Always,
    /// Returns the running operation with the same key instead of starting another.
    Join(String),
    /// Fails with `FAILED_PRECONDITION` and `conflict` while an operation with the same key runs.
    Exclusive {
        key: String,
        conflict: String,
    },
}

pub(crate) struct Started {
    pub(crate) operation: Operation,
    /// The operation was already running; its starter owns its cancellation.
    pub(crate) joined: bool,
}

pub(crate) enum Followed {
    Event(Event),
    /// Last item: the operation in its final state.
    Finished(Operation),
}

#[derive(Clone)]
enum Change {
    Upserted(Operation),
    Removed(String),
}

/// Every operation of this daemon process, running and recently finished.
pub(crate) struct Operations {
    state: Mutex<State>,
    changes: broadcast::Sender<Change>,
    /// Keeps IDs unique across daemon processes.
    prefix: String,
    /// Cancels every cancellable operation once the daemon begins to stop.
    stopping: CancellationToken,
}

#[derive(Default)]
struct State {
    /// In the order the operations started.
    entries: Vec<Entry>,
    next: u64,
    /// IDs of finished operations, oldest first.
    finished: VecDeque<String>,
}

struct Entry {
    summary: Operation,
    key: Option<String>,
    log: Log,
    /// `None` once the operation finished, which ends every follower after the events it has not read yet.
    live: Option<broadcast::Sender<Event>>,
    cancel: CancellationToken,
    published: Option<Instant>,
    flush_pending: bool,
}

impl Operations {
    pub(crate) fn new(stopping: CancellationToken) -> Arc<Self> {
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_millis())
            .unwrap_or_default();

        Arc::new(Self {
            state: Mutex::default(),
            changes: broadcast::Sender::new(WATCH_EVENTS),
            prefix: format!("{started:x}"),
            stopping,
        })
    }

    /// Starts `work` on its own task unless `admission` returns a running operation or refuses. A cancelled
    /// operation's work future is dropped.
    pub(crate) fn start<W, F>(
        self: &Arc<Self>,
        request: OperationRequest,
        cancellable: bool,
        admission: Admission,
        work: W,
    ) -> Result<Started, Status>
    where
        W: FnOnce(Sink) -> F,
        F: Future<Output = Result<(), Status>> + Send + 'static,
    {
        let mut state = self.lock();
        let key = match admission {
            Admission::Always => None,
            Admission::Join(key) => {
                if let Some(running) = state.running(&key) {
                    return Ok(Started {
                        operation: running.summary.clone(),
                        joined: true,
                    });
                }
                Some(key)
            }
            Admission::Exclusive { key, conflict } => {
                if state.running(&key).is_some() {
                    return Err(Status::failed_precondition(conflict));
                }
                Some(key)
            }
        };

        let id = format!("{}-{}", self.prefix, state.next);
        state.next += 1;
        let summary = Operation {
            id: id.clone(),
            request: Some(request),
            state: OperationState::Running.into(),
            progress: None,
            started_at: Some(SystemTime::now().into()),
            finished_at: None,
            cancellable,
            error: None,
        };
        let cancel = if cancellable {
            self.stopping.child_token()
        } else {
            CancellationToken::new()
        };
        state.entries.push(Entry {
            summary: summary.clone(),
            key,
            log: Log::default(),
            live: Some(broadcast::Sender::new(LIVE_EVENTS)),
            cancel: cancel.clone(),
            published: None,
            flush_pending: false,
        });
        self.send(Change::Upserted(summary.clone()));
        drop(state);

        let work = work(Sink {
            operations: Arc::clone(self),
            id: id.clone(),
        });
        let operations = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = tokio::select! {
                result = work => match result {
                    Ok(()) => Outcome::Succeeded,
                    Err(status) => Outcome::Failed(status),
                },
                () = cancel.cancelled() => Outcome::Cancelled,
            };
            operations.finish(&id, &outcome);
        });

        Ok(Started {
            operation: summary,
            joined: false,
        })
    }

    /// Every operation, then every change; a snapshot again whenever the receiver falls behind.
    pub(crate) fn watch(self: &Arc<Self>) -> BoxStream<'static, watch_operations_response::Event> {
        let operations = Arc::clone(self);
        async_stream::stream! {
            loop {
                let (snapshot, mut changes) = {
                    let state = operations.lock();
                    let snapshot = state.entries.iter().map(|entry| entry.summary.clone()).collect();
                    (snapshot, operations.changes.subscribe())
                };
                yield watch_operations_response::Event::Snapshot(OperationSnapshot { operations: snapshot });
                loop {
                    match changes.recv().await {
                        Ok(Change::Upserted(operation)) => yield watch_operations_response::Event::Upserted(operation),
                        Ok(Change::Removed(id)) => yield watch_operations_response::Event::RemovedId(id),
                        Err(RecvError::Lagged(_)) => break,
                        Err(RecvError::Closed) => return,
                    }
                }
            }
        }
        .boxed()
    }

    /// The events kept for replay, then new ones, then the final state; fails with `NOT_FOUND` for an operation no
    /// longer kept.
    pub(crate) fn follow(self: &Arc<Self>, id: &str) -> Result<BoxStream<'static, Followed>, Status> {
        let state = self.lock();
        let entry = state.get(id).ok_or_else(|| not_found(id))?;
        let replay = entry.log.replay();
        let live = entry.live.as_ref().map(broadcast::Sender::subscribe);
        let last_known = entry.summary.clone();
        drop(state);

        let operations = Arc::clone(self);
        let id = id.to_owned();
        let live = stream::iter(live)
            .flat_map(|receiver| BroadcastStream::new(receiver).filter_map(|event| async move { event.ok() }));
        let finished = stream::once(async move { Followed::Finished(operations.summary(&id).unwrap_or(last_known)) });

        Ok(stream::iter(replay)
            .chain(live)
            .map(Followed::Event)
            .chain(finished)
            .boxed())
    }

    /// Cancels a running operation; does nothing for a finished one.
    pub(crate) fn cancel(&self, id: &str) -> Result<(), Status> {
        let state = self.lock();
        let entry = state.get(id).ok_or_else(|| not_found(id))?;
        if entry.summary.state() != OperationState::Running {
            return Ok(());
        }
        if !entry.summary.cancellable {
            return Err(Status::failed_precondition("this operation cannot be cancelled"));
        }
        entry.cancel.cancel();

        Ok(())
    }

    fn summary(&self, id: &str) -> Option<Operation> {
        self.lock().get(id).map(|entry| entry.summary.clone())
    }

    fn finish(self: &Arc<Self>, id: &str, outcome: &Outcome) {
        let mut state = self.lock();
        let Some(entry) = state.get_mut(id) else {
            return;
        };
        let (operation_state, error) = match outcome {
            Outcome::Succeeded => (OperationState::Succeeded, None),
            Outcome::Failed(status) => (OperationState::Failed, Some(status::operation_error(status))),
            Outcome::Cancelled => (OperationState::Cancelled, None),
        };
        entry.summary.state = operation_state.into();
        entry.summary.error = error;
        entry.summary.finished_at = Some(SystemTime::now().into());
        entry.live = None;
        let summary = entry.summary.clone();
        self.send(Change::Upserted(summary));

        state.finished.push_back(id.to_owned());
        while state.finished.len() > MAX_FINISHED {
            if let Some(oldest) = state.finished.pop_front() {
                state.entries.retain(|entry| entry.summary.id != oldest);
                self.send(Change::Removed(oldest));
            }
        }
        drop(state);

        let operations = Arc::downgrade(self);
        let id = id.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(RETENTION).await;
            if let Some(operations) = Weak::upgrade(&operations) {
                operations.remove(&id);
            }
        });
    }

    fn remove(&self, id: &str) {
        let mut state = self.lock();
        let before = state.entries.len();
        state.entries.retain(|entry| entry.summary.id != id);
        if state.entries.len() != before {
            state.finished.retain(|finished| finished != id);
            self.send(Change::Removed(id.to_owned()));
        }
    }

    /// Publishes the latest step of a running operation whose publication `Sink::progress` held back.
    fn flush(&self, id: &str) {
        let mut state = self.lock();
        let Some(entry) = state.get_mut(id) else {
            return;
        };
        entry.flush_pending = false;
        if entry.summary.state() == OperationState::Running {
            entry.published = Some(Instant::now());
            self.send(Change::Upserted(entry.summary.clone()));
        }
    }

    /// Sends under the state lock, so a watcher's snapshot and the changes after it never overlap or miss one.
    fn send(&self, change: Change) {
        let _ = self.changes.send(change);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.summary.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|entry| entry.summary.id == id)
    }

    fn running(&self, key: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|entry| entry.key.as_deref() == Some(key) && entry.summary.state() == OperationState::Running)
    }
}

enum Outcome {
    Succeeded,
    Failed(Status),
    Cancelled,
}

/// Where an operation's work reports its events and steps.
#[derive(Clone)]
pub(crate) struct Sink {
    operations: Arc<Operations>,
    id: String,
}

impl Sink {
    /// Keeps `event` for replay and sends it to every follower.
    pub(crate) fn emit(&self, event: Event) {
        let mut state = self.operations.lock();
        if let Some(entry) = state.get_mut(&self.id) {
            entry.log.record(&event);
            if let Some(live) = &entry.live {
                let _ = live.send(event);
            }
        }
    }

    /// Makes `progress` the operation's latest step; watchers hear of it at most every [`PROGRESS_INTERVAL`].
    pub(crate) fn progress(&self, progress: Progress) {
        let mut state = self.operations.lock();
        let Some(entry) = state.get_mut(&self.id) else {
            return;
        };
        entry.summary.progress = Some(progress);

        let now = Instant::now();
        let wait = entry
            .published
            .map(|published| PROGRESS_INTERVAL.saturating_sub(now.duration_since(published)))
            .unwrap_or_default();
        if wait.is_zero() {
            entry.published = Some(now);
            self.operations.send(Change::Upserted(entry.summary.clone()));
        } else if !entry.flush_pending {
            entry.flush_pending = true;
            let operations = Arc::clone(&self.operations);
            let id = self.id.clone();
            tokio::spawn(async move {
                tokio::time::sleep(wait).await;
                operations.flush(&id);
            });
        }
    }
}

/// Events kept to rebuild an operation's progress for a follower that arrives late.
#[derive(Default)]
struct Log {
    /// The latest event for each key, in the order the keys first appeared.
    latest: Vec<(String, Event)>,
    rest: VecDeque<Event>,
    lines: usize,
}

impl Log {
    fn record(&mut self, event: &Event) {
        if let Some(key) = latest_key(event) {
            match self.latest.iter_mut().find(|(existing, _)| *existing == key) {
                Some((_, kept)) => kept.clone_from(event),
                None => self.latest.push((key, event.clone())),
            }
            return;
        }

        self.rest.push_back(event.clone());
        if is_line(event) {
            self.lines += 1;
            if self.lines > REPLAY_LINES
                && let Some(oldest) = self.rest.iter().position(is_line)
            {
                self.rest.remove(oldest);
                self.lines -= 1;
            }
        }
    }

    fn replay(&self) -> Vec<Event> {
        self.latest
            .iter()
            .map(|(_, event)| event.clone())
            .chain(self.rest.iter().cloned())
            .collect()
    }
}

/// Key of the events of which replay keeps only the latest.
fn latest_key(event: &Event) -> Option<String> {
    use contr_api::diagnostics::v1::create_bundle_response;
    use contr_api::images::v1::{pull_image_response, push_image_response};

    match event {
        Event::PullImage(pull) => match &pull.event {
            Some(pull_image_response::Event::Layer(layer)) => Some(format!("layer {}", layer.layer_id)),
            _ => None,
        },
        Event::PushImage(push) => match push.event {
            Some(push_image_response::Event::Status(_)) => Some("status".to_owned()),
            _ => None,
        },
        Event::CreateBundle(bundle) => match bundle.event {
            Some(create_bundle_response::Event::Progress(_)) => Some("progress".to_owned()),
            _ => None,
        },
        _ => None,
    }
}

/// Whether replay keeps only the last [`REPLAY_LINES`] of the events like `event`.
fn is_line(event: &Event) -> bool {
    use contr_api::compose::v1::run_action_response;
    use contr_api::images::v1::pull_image_response;

    match event {
        Event::ComposeAction(action) => matches!(action.event, Some(run_action_response::Event::Output(_))),
        Event::PullImage(pull) => matches!(pull.event, Some(pull_image_response::Event::Status(_))),
        _ => false,
    }
}

fn not_found(id: &str) -> Status {
    Status::not_found(format!("no operation {id} is kept"))
}

#[cfg(test)]
mod tests {
    use contr_api::compose::v1::{RunActionResponse, run_action_response};
    use contr_api::images::v1::{LayerProgress, PullImageRequest, PullImageResponse, pull_image_response};
    use contr_api::operations::v1::operation_request::Kind;
    use tokio::sync::oneshot;
    use tokio::time::timeout;
    use tonic::Code;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    fn pull(reference: &str) -> OperationRequest {
        OperationRequest {
            kind: Some(Kind::PullImage(PullImageRequest {
                reference: reference.to_owned(),
                platform: String::new(),
            })),
        }
    }

    fn layer(id: &str, current_bytes: u64) -> Event {
        Event::PullImage(PullImageResponse {
            event: Some(pull_image_response::Event::Layer(LayerProgress {
                layer_id: id.to_owned(),
                status: "Downloading".to_owned(),
                current_bytes,
                total_bytes: 100,
            })),
        })
    }

    fn compose_line(number: usize) -> Event {
        Event::ComposeAction(RunActionResponse {
            event: Some(run_action_response::Event::Output(format!("line {number}"))),
        })
    }

    fn pending(_: Sink) -> std::future::Pending<Result<(), Status>> {
        std::future::pending()
    }

    /// Follows `id` to its end: the events, then the final state.
    async fn finished(operations: &Arc<Operations>, id: &str) -> (Vec<Event>, Operation) {
        let mut events = Vec::new();
        let mut followed = operations.follow(id).unwrap();
        while let Some(item) = timeout(WAIT, followed.next()).await.unwrap() {
            match item {
                Followed::Event(event) => events.push(event),
                Followed::Finished(operation) => return (events, operation),
            }
        }
        panic!("the stream ended without the final state");
    }

    async fn next_change(
        changes: &mut BoxStream<'static, watch_operations_response::Event>,
    ) -> watch_operations_response::Event {
        timeout(WAIT, changes.next()).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn followers_get_the_latest_events_and_the_final_state() {
        let operations = Operations::new(CancellationToken::new());
        let (release, released) = oneshot::channel::<()>();
        let started = operations
            .start(pull("alpine"), true, Admission::Always, |sink| async move {
                sink.emit(layer("a", 10));
                let _ = released.await;
                sink.emit(layer("a", 100));
                Ok(())
            })
            .unwrap();

        let follower = tokio::spawn({
            let operations = Arc::clone(&operations);
            let id = started.operation.id.clone();
            async move { finished(&operations, &id).await }
        });
        release.send(()).unwrap();
        let (events, operation) = follower.await.unwrap();

        assert_eq!(events.last(), Some(&layer("a", 100)));
        assert_eq!(operation.state(), OperationState::Succeeded);
        assert!(operation.finished_at.is_some());
    }

    #[tokio::test]
    async fn a_start_with_the_key_of_a_running_operation_joins_it() {
        let operations = Operations::new(CancellationToken::new());
        let join = || Admission::Join("pull alpine".to_owned());

        let first = operations.start(pull("alpine"), true, join(), pending).unwrap();
        let second = operations.start(pull("alpine"), true, join(), pending).unwrap();

        assert!(!first.joined);
        assert!(second.joined);
        assert_eq!(second.operation.id, first.operation.id);
    }

    #[tokio::test]
    async fn an_exclusive_start_is_refused_while_its_key_runs() {
        let operations = Operations::new(CancellationToken::new());
        let exclusive = || Admission::Exclusive {
            key: "compose shop".to_owned(),
            conflict: "a Compose action already runs on project shop".to_owned(),
        };

        operations.start(pull("alpine"), true, exclusive(), pending).unwrap();
        let refused = operations.start(pull("alpine"), true, exclusive(), pending);

        assert_eq!(
            refused.err().map(|status| status.code()),
            Some(Code::FailedPrecondition)
        );
    }

    #[tokio::test]
    async fn cancelling_drops_the_work_and_ends_cancelled() {
        let operations = Operations::new(CancellationToken::new());
        let (held, dropped) = oneshot::channel::<()>();
        let started = operations
            .start(pull("alpine"), true, Admission::Always, move |_| async move {
                let _held = held;
                std::future::pending::<()>().await;
                Ok(())
            })
            .unwrap();

        operations.cancel(&started.operation.id).unwrap();
        let _ = timeout(WAIT, dropped).await.unwrap();
        let (_, operation) = finished(&operations, &started.operation.id).await;

        assert_eq!(operation.state(), OperationState::Cancelled);
    }

    #[tokio::test]
    async fn an_operation_that_cannot_be_cancelled_refuses() {
        let operations = Operations::new(CancellationToken::new());
        let started = operations
            .start(pull("alpine"), false, Admission::Always, pending)
            .unwrap();

        let refused = operations.cancel(&started.operation.id);

        assert_eq!(
            refused.err().map(|status| status.code()),
            Some(Code::FailedPrecondition)
        );
        assert_eq!(
            operations.cancel("unknown").err().map(|status| status.code()),
            Some(Code::NotFound)
        );
    }

    #[tokio::test]
    async fn a_stopping_daemon_cancels_what_can_be_cancelled() {
        let stopping = CancellationToken::new();
        let operations = Operations::new(stopping.clone());
        let started = operations
            .start(pull("alpine"), true, Admission::Always, pending)
            .unwrap();

        stopping.cancel();
        let (_, operation) = finished(&operations, &started.operation.id).await;

        assert_eq!(operation.state(), OperationState::Cancelled);
    }

    #[tokio::test]
    async fn a_failure_keeps_the_status_code_and_message() {
        let operations = Operations::new(CancellationToken::new());
        let started = operations
            .start(pull("missing"), true, Admission::Always, |_| async {
                Err(Status::not_found("no such image"))
            })
            .unwrap();

        let (_, operation) = finished(&operations, &started.operation.id).await;
        let error = operation.error.clone().unwrap();

        assert_eq!(operation.state(), OperationState::Failed);
        assert_eq!(error.code, i32::from(Code::NotFound));
        assert_eq!(error.message, "no such image");
    }

    #[tokio::test]
    async fn watchers_get_a_snapshot_then_each_change() {
        let operations = Operations::new(CancellationToken::new());
        let mut changes = operations.watch();

        let snapshot = next_change(&mut changes).await;
        let started = operations
            .start(pull("alpine"), true, Admission::Always, |_| async { Ok(()) })
            .unwrap();
        let running = next_change(&mut changes).await;
        let done = next_change(&mut changes).await;

        assert_eq!(
            snapshot,
            watch_operations_response::Event::Snapshot(OperationSnapshot::default())
        );
        let watch_operations_response::Event::Upserted(running) = running else {
            panic!("expected the started operation, got {running:?}");
        };
        let watch_operations_response::Event::Upserted(done) = done else {
            panic!("expected the finished operation, got {done:?}");
        };
        assert_eq!(running.id, started.operation.id);
        assert_eq!(running.state(), OperationState::Running);
        assert_eq!(done.state(), OperationState::Succeeded);
    }

    #[tokio::test]
    async fn only_the_most_recent_finished_operations_are_kept() {
        let operations = Operations::new(CancellationToken::new());
        let mut ids = Vec::new();
        for _ in 0..MAX_FINISHED + 2 {
            let started = operations
                .start(pull("alpine"), true, Admission::Always, |_| async { Ok(()) })
                .unwrap();
            let _ = finished(&operations, &started.operation.id).await;
            ids.push(started.operation.id);
        }

        assert!(operations.follow(&ids[0]).is_err());
        assert!(operations.follow(&ids[1]).is_err());
        assert!(operations.follow(&ids[2]).is_ok());
        assert_eq!(operations.lock().entries.len(), MAX_FINISHED);
    }

    #[test]
    fn replay_keeps_the_latest_progress_of_each_layer_and_the_last_lines() {
        let mut log = Log::default();
        log.record(&layer("a", 10));
        log.record(&layer("b", 5));
        log.record(&layer("a", 60));
        for number in 0..REPLAY_LINES + 5 {
            log.record(&compose_line(number));
        }

        let replay = log.replay();

        assert_eq!(replay.len(), 2 + REPLAY_LINES);
        assert_eq!(replay[0], layer("a", 60));
        assert_eq!(replay[1], layer("b", 5));
        assert_eq!(replay[2], compose_line(5));
    }
}
