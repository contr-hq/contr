//! Engine access from the user's own WSL distributions: while the engine runs, one `contr-agent integrate` per
//! requested distribution, restarted when it ends, which also restarts a distribution the user terminated.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use contr_backend::IntegrationState;
use contr_core::guest::{self, IntegrationConfig};
use contr_core::proxy::Proxy;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, sleep, timeout};

use crate::output::{LatestLine, forward_log, wait_for_line};
use crate::wsl;

const READY_WAIT: Duration = Duration::from_secs(60);
const STOP_WAIT: Duration = Duration::from_secs(10);
const STABLE_AFTER: Duration = Duration::from_secs(60);
const FIRST_RETRY: Duration = Duration::from_secs(2);
const MAX_RETRY: Duration = Duration::from_secs(60);

type States = Arc<Mutex<HashMap<String, IntegrationState>>>;

/// The requested distributions and the state of their integrations, shared by the backend and its sessions.
pub(crate) struct Integrations {
    requested: watch::Sender<BTreeSet<String>>,
    states: States,
}

impl Integrations {
    pub(crate) fn new(requested: BTreeSet<String>) -> Self {
        Self {
            requested: watch::Sender::new(requested),
            states: States::default(),
        }
    }

    pub(crate) fn set(&self, name: &str, integrated: bool) {
        self.requested.send_if_modified(|requested| {
            if integrated {
                requested.insert(name.to_owned())
            } else {
                requested.remove(name)
            }
        });
    }

    /// Requested distributions with the state of their integration.
    pub(crate) fn states(&self) -> Vec<(String, IntegrationState)> {
        let requested = self.requested.borrow().clone();
        requested
            .into_iter()
            .map(|name| {
                let state = self.state(&name);
                (name, state)
            })
            .collect()
    }

    pub(crate) fn state(&self, name: &str) -> IntegrationState {
        if !self.requested.borrow().contains(name) {
            return IntegrationState::Disabled;
        }

        lock(&self.states)
            .get(name)
            .cloned()
            .unwrap_or(IntegrationState::Pending)
    }

    /// Serves the requested distributions from the engine in distribution `runtime` until the result stops; new
    /// containers and builds of those distributions receive `container_proxy`.
    pub(crate) fn run(&self, runtime: &str, container_proxy: Option<Proxy>) -> Running {
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(reconcile(
            self.requested.subscribe(),
            Arc::clone(&self.states),
            runtime.to_owned(),
            IntegrationConfig { container_proxy },
            stopped,
        ));

        Running { stop, task }
    }
}

/// The integrations serving one engine session.
pub(crate) struct Running {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Running {
    /// Ends every integration; each removes what it added to its distribution.
    pub(crate) async fn stop(self) {
        self.stop.send_replace(true);
        if let Err(error) = self.task.await {
            tracing::warn!(%error, "WSL integrations ended abnormally");
        }
    }
}

async fn reconcile(
    mut requested: watch::Receiver<BTreeSet<String>>,
    states: States,
    runtime: String,
    config: IntegrationConfig,
    mut stopped: watch::Receiver<bool>,
) {
    let mut running: HashMap<String, watch::Sender<bool>> = HashMap::new();
    let mut supervisors = JoinSet::new();
    loop {
        let wanted = requested.borrow_and_update().clone();
        running.retain(|name, stop| {
            let keep = wanted.contains(name);
            if !keep {
                stop.send_replace(true);
                lock(&states).remove(name);
            }
            keep
        });
        for name in wanted {
            running.entry(name.clone()).or_insert_with(|| {
                let (stop, stopped) = watch::channel(false);
                supervisors.spawn(supervise(
                    name,
                    runtime.clone(),
                    config.clone(),
                    Arc::clone(&states),
                    stopped,
                ));
                stop
            });
        }

        tokio::select! {
            changed = requested.changed() => if changed.is_err() {
                break;
            },
            _ = stopped.changed() => break,
            Some(_) = supervisors.join_next(), if !supervisors.is_empty() => {}
        }
    }

    for stop in running.values() {
        stop.send_replace(true);
    }
    while supervisors.join_next().await.is_some() {}
    lock(&states).clear();
}

/// Keeps distribution `name` integrated with `config` until `stop` fires, retrying with a growing delay after each
/// failure.
async fn supervise(
    name: String,
    runtime: String,
    config: IntegrationConfig,
    states: States,
    mut stop: watch::Receiver<bool>,
) {
    let publish = |state| {
        lock(&states).insert(name.clone(), state);
    };
    let mut failures = 0;
    loop {
        publish(IntegrationState::Pending);
        let attempt = Instant::now();
        let started = tokio::select! {
            started = Integration::start(&name, &config) => started,
            _ = stop.changed() => {
                release(&runtime, &name).await;
                return;
            }
        };
        let failure = match started {
            Ok(mut integration) => {
                publish(IntegrationState::Active);
                tokio::select! {
                    reason = integration.ended() => reason,
                    _ = stop.changed() => {
                        integration.stop().await;
                        return;
                    }
                }
            }
            Err(error) => error.to_string(),
        };

        tracing::warn!(distribution = %name, reason = %failure, "WSL integration ended");
        publish(IntegrationState::Failed(failure));
        release(&runtime, &name).await;
        if !is_registered(&name).await {
            tracing::info!(distribution = %name, "the distribution is no longer registered");
            return;
        }
        if attempt.elapsed() >= STABLE_AFTER {
            failures = 0;
        }
        let delay = retry_delay(failures);
        failures += 1;
        tokio::select! {
            () = sleep(delay) => {}
            _ = stop.changed() => return,
        }
    }
}

/// Unmounts the distribution's root file system that an integration ending without cleanup left in every
/// distribution.
async fn release(runtime: &str, name: &str) {
    if let Err(error) = wsl::exec(runtime, &[guest::AGENT_PATH, "release-distribution", name]).await {
        tracing::warn!(%error, distribution = %name, "cannot release the distribution's root file system");
    }
}

/// Whether distribution `name` is still registered; a failed listing counts as registered so a retry follows.
async fn is_registered(name: &str) -> bool {
    wsl::distributions().await.map_or(true, |names| {
        names.iter().any(|listed| listed.eq_ignore_ascii_case(name))
    })
}

fn retry_delay(failures: u32) -> Duration {
    FIRST_RETRY.saturating_mul(1 << failures.min(6)).min(MAX_RETRY)
}

fn lock(states: &States) -> std::sync::MutexGuard<'_, HashMap<String, IntegrationState>> {
    states.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running `contr-agent integrate` in one distribution. Its standard input lives outside `child`, whose `wait`
/// would close it, and the integration ends when it closes.
struct Integration {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr: LatestLine,
    _stdout: Lines<BufReader<ChildStdout>>,
    _log: JoinHandle<()>,
}

impl Integration {
    async fn start(name: &str, config: &IntegrationConfig) -> io::Result<Self> {
        let owner = wsl::default_uid(name).await?.to_string();
        let agent = format!("{}/bin/{}", guest::INTEGRATION_DIR, contr_core::product::AGENT_BINARY);
        let mut child = wsl::spawn(name, &[&agent, "integrate", "--distribution", name, "--owner", &owner])?;
        let stderr = LatestLine::default();
        let log = forward_log(child.stderr.take(), name, Some(Arc::clone(&stderr)));
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("the integration has no standard output"))?;
        let mut stdout = BufReader::new(stdout).lines();
        let mut stdin = child.stdin.take();
        // The configuration may hold proxy credentials, which a command line would show to every user of the
        // distribution.
        if let Some(stdin) = stdin.as_mut() {
            let mut line = serde_json::to_vec(config)?;
            line.push(b'\n');
            stdin.write_all(&line).await?;
        }

        let ready = matches!(
            timeout(READY_WAIT, wait_for_line(&mut stdout, guest::INTEGRATION_READY)).await,
            Ok(Ok(true))
        );
        if !ready {
            let _ = child.kill().await;
            log.await.ok();
            return Err(io::Error::other(explanation(&stderr, "the integration did not start")));
        }

        Ok(Self {
            child,
            stdin,
            stderr,
            _stdout: stdout,
            _log: log,
        })
    }

    /// Waits for the integration to end on its own and says why it did.
    async fn ended(&mut self) -> String {
        match self.child.wait().await {
            Ok(status) if status.success() => "the integration stopped".to_owned(),
            Ok(status) => explanation(&self.stderr, &format!("the integration exited with {status}")),
            Err(error) => format!("the integration failed: {error}"),
        }
    }

    /// Closes standard input, on which the integration removes what it added, and kills it if it does not exit.
    async fn stop(mut self) {
        drop(self.stdin.take());
        if timeout(STOP_WAIT, self.child.wait()).await.is_err() {
            let _ = self.child.kill().await;
        }
    }
}

/// The error the agent reported as it exited, which is its last line on standard error, or `fallback`.
fn explanation(stderr: &LatestLine, fallback: &str) -> String {
    let latest = stderr.lock().unwrap_or_else(PoisonError::into_inner);

    latest
        .trim()
        .strip_prefix("Error: ")
        .map_or_else(|| fallback.to_owned(), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_sooner_after_fewer_failures() {
        let delays: Vec<_> = (0..8).map(|failures| retry_delay(failures).as_secs()).collect();

        assert_eq!(delays, [2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn reports_requested_distributions_as_pending_until_they_run() {
        let integrations = Integrations::new(BTreeSet::from(["Ubuntu".to_owned()]));

        assert_eq!(integrations.state("Ubuntu"), IntegrationState::Pending);
        assert_eq!(integrations.state("Debian"), IntegrationState::Disabled);

        integrations.set("Debian", true);
        integrations.set("Ubuntu", false);
        assert_eq!(integrations.state("Debian"), IntegrationState::Pending);
        assert_eq!(integrations.state("Ubuntu"), IntegrationState::Disabled);
    }

    #[test]
    fn explains_failures_with_the_reported_error() {
        let stderr = LatestLine::default();
        assert_eq!(explanation(&stderr, "exited with 1"), "exited with 1");

        *stderr.lock().unwrap() = "2026-10-20T09:47:08Z  INFO contr_agent::integrate: serving the engine".to_owned();
        assert_eq!(explanation(&stderr, "exited with 1"), "exited with 1");

        *stderr.lock().unwrap() = "Error: another engine serves /var/run/docker.sock".to_owned();
        assert_eq!(
            explanation(&stderr, "exited with 1"),
            "another engine serves /var/run/docker.sock"
        );
    }
}
