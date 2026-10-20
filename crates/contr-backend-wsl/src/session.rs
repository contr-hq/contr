use std::io;
use std::sync::Arc;
use std::time::Duration;

use contr_core::guest;
use contr_engine_proxy::Rewrites;
use contr_mux::{Channel, Opener};
use contr_transport::{Endpoint, Listener};
use futures_util::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, ChildStdout};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, sleep, timeout};

use crate::{ping, wsl};

const ANCHOR_WAIT: Duration = Duration::from_secs(60);
const ENGINE_WAIT: Duration = Duration::from_secs(120);
const PING_WAIT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const AGENT_STOP: Duration = Duration::from_secs(60);
const ANCHOR_STOP: Duration = Duration::from_secs(10);

/// Running engine: the data anchor, the agent, the multiplexed session to the agent, and the per-user engine pipe
/// that relays Docker API connections into that session.
pub(crate) struct Session {
    anchor: Child,
    agent: Child,
    opener: Opener,
    relay: JoinHandle<()>,
    background: Vec<JoinHandle<()>>,
}

impl Session {
    /// Anchors the data distribution, starts the agent in the runtime distribution and serves `engine_pipe`;
    /// returns once the engine answers a ping.
    pub(crate) async fn start(runtime: &str, data: &str, engine_pipe: &str) -> io::Result<Self> {
        let mut background = Vec::new();

        let mut anchor = wsl::spawn(data, &[guest::AGENT_PATH, "data-anchor"])?;
        background.push(forward_log(anchor.stderr.take(), data));
        wait_for_line(anchor.stdout.take(), guest::DATA_READY).await?;

        let mut agent = wsl::spawn(runtime, &[guest::AGENT_PATH, "serve"])?;
        background.push(forward_log(agent.stderr.take(), runtime));
        let (Some(stdout), Some(stdin)) = (agent.stdout.take(), agent.stdin.take()) else {
            return Err(io::Error::other("the agent has no standard streams"));
        };
        let (opener, driver) = contr_mux::connect(tokio::io::join(stdout, stdin));
        background.push(tokio::spawn(async move {
            if let Err(error) = driver.await {
                tracing::warn!(%error, "the guest session failed");
            }
        }));

        let listener = Listener::bind_messages(&Endpoint::NamedPipe(engine_pipe.to_owned()))
            .map_err(|error| io::Error::other(format!("cannot serve {engine_pipe}: {error}")))?;
        let relay = relay(listener, opener.clone());

        let mut session = Self {
            anchor,
            agent,
            opener,
            relay,
            background,
        };
        if let Err(error) = session.wait_for_engine().await {
            session.stop().await;
            return Err(error);
        }

        Ok(session)
    }

    /// Whether the agent and the data anchor are still running.
    pub(crate) fn is_alive(&mut self) -> bool {
        matches!(self.agent.try_wait(), Ok(None)) && matches!(self.anchor.try_wait(), Ok(None))
    }

    /// Stops serving the pipe, closes the session so the agent stops the engine, then releases the data anchor.
    pub(crate) async fn stop(mut self) {
        self.relay.abort();
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

fn forward_log(stderr: Option<impl AsyncRead + Unpin + Send + 'static>, distribution: &str) -> JoinHandle<()> {
    let distribution = distribution.to_owned();
    tokio::spawn(async move {
        let Some(stderr) = stderr else {
            return;
        };
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(target: "contr_agent", %distribution, "{line}");
        }
    })
}

async fn wait_for_line(stdout: Option<ChildStdout>, expected: &str) -> io::Result<()> {
    let stdout = stdout.ok_or_else(|| io::Error::other("the data anchor has no standard output"))?;
    let mut lines = BufReader::new(stdout).lines();
    let ready = async {
        while let Some(line) = lines.next_line().await? {
            if line.trim() == expected {
                return Ok(());
            }
        }
        Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the data anchor exited before mounting engine data",
        ))
    };

    timeout(ANCHOR_WAIT, ready).await.map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "the data anchor did not mount engine data in time",
        )
    })?
}
