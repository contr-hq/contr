use std::io;
use std::sync::Arc;
use std::time::Duration;

use contr_core::guest;
use contr_engine_proxy::Rewrites;
use contr_mux::{Channel, Opener, Role};
use contr_transport::{Endpoint, Listener};
use futures_util::StreamExt;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, sleep, timeout};

use crate::integration::{Integrations, Running};
use crate::output::{forward_log, wait_for_line};
use crate::{host_tcp, ping, wsl};

const ANCHOR_WAIT: Duration = Duration::from_secs(60);
const ENGINE_WAIT: Duration = Duration::from_secs(120);
const PING_WAIT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const AGENT_STOP: Duration = Duration::from_secs(60);
const ANCHOR_STOP: Duration = Duration::from_secs(10);

/// Running engine: the data anchor, the agent, the multiplexed session to the agent, the per-user engine pipe that
/// relays Docker API connections into that session, and the integrations of the user's distributions.
pub(crate) struct Session {
    anchor: Child,
    agent: Child,
    opener: Opener,
    relays: Vec<JoinHandle<()>>,
    shared_pipes: Vec<String>,
    integrations: Option<Running>,
    background: Vec<JoinHandle<()>>,
}

impl Session {
    /// Anchors the data distribution, starts the agent in the runtime distribution, serves `engine_pipe` and each of
    /// `shared_pipes` no other program owns, and returns once the engine answers a ping; the requested
    /// `integrations` then run until the session stops.
    pub(crate) async fn start(
        runtime: &str,
        data: &str,
        engine_pipe: &str,
        shared_pipes: &[String],
        integrations: &Integrations,
    ) -> io::Result<Self> {
        let mut background = Vec::new();

        let mut anchor = wsl::spawn(data, &[guest::AGENT_PATH, "data-anchor"])?;
        background.push(forward_log(anchor.stderr.take(), data, None));
        wait_for_data(anchor.stdout.take()).await?;

        let mut agent = wsl::spawn(runtime, &[guest::AGENT_PATH, "serve", "--share"])?;
        background.push(forward_log(agent.stderr.take(), runtime, None));
        let (Some(stdout), Some(stdin)) = (agent.stdout.take(), agent.stdin.take()) else {
            return Err(io::Error::other("the agent has no standard streams"));
        };
        let (opener, driver) = contr_mux::session(tokio::io::join(stdout, stdin), Role::Host, host_tcp::serve);
        background.push(tokio::spawn(async move {
            if let Err(error) = driver.await {
                tracing::warn!(%error, "the guest session failed");
            }
        }));

        let listener = Listener::bind_messages(&Endpoint::NamedPipe(engine_pipe.to_owned()))
            .map_err(|error| io::Error::other(format!("cannot serve {engine_pipe}: {error}")))?;
        let mut relays = vec![relay(listener, opener.clone())];
        let mut served = Vec::new();
        for pipe in shared_pipes {
            match Listener::bind_messages(&Endpoint::NamedPipe(pipe.clone())) {
                Ok(listener) => {
                    tracing::info!(%pipe, "serving the engine");
                    relays.push(relay(listener, opener.clone()));
                    served.push(pipe.clone());
                }
                Err(error) => tracing::info!(%error, %pipe, "another program serves this pipe"),
            }
        }

        let mut session = Self {
            anchor,
            agent,
            opener,
            relays,
            shared_pipes: served,
            integrations: None,
            background,
        };
        if let Err(error) = session.wait_for_engine().await {
            session.stop().await;
            return Err(error);
        }
        session.integrations = Some(integrations.run(runtime));

        Ok(session)
    }

    /// Whether this session serves `pipe`, one of the shared pipes it was asked to serve.
    pub(crate) fn serves(&self, pipe: &str) -> bool {
        self.shared_pipes.iter().any(|served| served == pipe)
    }

    /// Whether the agent and the data anchor are still running.
    pub(crate) fn is_alive(&mut self) -> bool {
        matches!(self.agent.try_wait(), Ok(None)) && matches!(self.anchor.try_wait(), Ok(None))
    }

    /// Ends the integrations, stops serving the pipes, closes the session so the agent stops the engine, then
    /// releases the data anchor.
    pub(crate) async fn stop(mut self) {
        if let Some(integrations) = self.integrations.take() {
            integrations.stop().await;
        }
        for relay in &self.relays {
            relay.abort();
        }
        drop(self.opener);

        if timeout(AGENT_STOP, self.agent.wait()).await.is_err() {
            tracing::warn!("the agent did not stop in time; killing it");
            let _ = self.agent.kill().await;
        }
        drop(self.anchor.stdin.take());
        if timeout(ANCHOR_STOP, self.anchor.wait()).await.is_err() {
            let _ = self.anchor.kill().await;
        }
        for task in self.background {
            task.abort();
        }
    }

    async fn wait_for_engine(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + ENGINE_WAIT;
        loop {
            if self.engine_answers().await {
                return Ok(());
            }
            if let Some(status) = self.agent.try_wait()? {
                return Err(io::Error::other(format!("the agent exited with {status}")));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the engine did not answer in time",
                ));
            }
            sleep(POLL_INTERVAL).await;
        }
    }

    async fn engine_answers(&self) -> bool {
        let attempt = async {
            let stream = self.opener.open(Channel::Engine).await.ok()?;
            ping::engine_answers(stream).await.ok()
        };

        matches!(timeout(PING_WAIT, attempt).await, Ok(Some(true)))
    }
}

fn relay(listener: Listener, opener: Opener) -> JoinHandle<()> {
    let rewrites = Arc::new(Rewrites {
        drives_root: guest::WINDOWS_DRIVES.to_owned(),
        distro_root: None,
        host_alias: true,
    });
    tokio::spawn(async move {
        let mut incoming = listener.incoming();
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = incoming.next() => match accepted {
                    Some(Ok(client)) => {
                        let opener = opener.clone();
                        let connect = move || {
                            let opener = opener.clone();
                            async move { opener.open(Channel::Engine).await.map_err(io::Error::other) }
                        };
                        connections.spawn(contr_engine_proxy::serve(client, connect, Arc::clone(&rewrites)));
                    }
                    Some(Err(error)) => tracing::warn!(%error, "cannot accept an engine pipe client"),
                    None => break,
                },
                Some(served) = connections.join_next(), if !connections.is_empty() => {
                    if let Ok(Err(error)) = served {
                        tracing::debug!(%error, "engine pipe client ended");
                    }
                }
            }
        }
    })
}

async fn wait_for_data(stdout: Option<ChildStdout>) -> io::Result<()> {
    let stdout = stdout.ok_or_else(|| io::Error::other("the data anchor has no standard output"))?;
    let mut lines = BufReader::new(stdout).lines();

    match timeout(ANCHOR_WAIT, wait_for_line(&mut lines, guest::DATA_READY)).await {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the data anchor exited before mounting engine data",
        )),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the data anchor did not mount engine data in time",
        )),
    }
}
