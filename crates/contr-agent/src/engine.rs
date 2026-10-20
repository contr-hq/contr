use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use contr_core::guest;
use contr_mux::{Channel, MuxStream};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::signal::unix::{SignalKind, signal};

use crate::mounts::is_mount_point;
use crate::network;

const DOCKERD: &str = "/usr/local/bin/dockerd";
const LOG_DIR: &str = "/var/log/contr";
const SEARCH_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const STOP_GRACE: Duration = Duration::from_secs(30);
const STABLE_AFTER: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Runs `dockerd` and relays multiplexed engine streams to its socket until the host closes the session or the agent
/// receives SIGTERM, then stops `dockerd`.
pub(crate) async fn serve(data_root: Option<PathBuf>) -> anyhow::Result<()> {
    let data_root = match data_root {
        Some(path) => path,
        None => default_data_root()?,
    };
    let mut engine = Engine::start(data_root)?;
    let mut session = tokio::spawn(contr_mux::serve(
        tokio::io::join(tokio::io::stdin(), tokio::io::stdout()),
        relay,
    ));
    let mut terminate = signal(SignalKind::terminate())?;

    loop {
        tokio::select! {
            ended = &mut session => {
                match ended {
                    Ok(Ok(())) => tracing::info!("host closed the session"),
                    Ok(Err(error)) => tracing::warn!(%error, "host session failed"),
                    Err(error) => tracing::warn!(%error, "host session task failed"),
                }
                break;
            }
            _ = terminate.recv() => break,
            status = engine.wait() => engine.restart(status?).await?,
        }
    }
    engine.stop().await;
    Ok(())
}

fn default_data_root() -> anyhow::Result<PathBuf> {
    let mount = Path::new(guest::DATA_MOUNT);
    if !is_mount_point(mount)? {
        bail!(
            "{} is not mounted; the data guest must be anchored first",
            mount.display()
        );
    }

    Ok(mount.join("docker"))
}

async fn relay(channel: Channel, mut stream: MuxStream) {
    match channel {
        Channel::Engine => match UnixStream::connect(guest::ENGINE_SOCKET).await {
            Ok(mut socket) => {
                if let Err(error) = tokio::io::copy_bidirectional(&mut stream, &mut socket).await {
                    tracing::debug!(%error, "engine stream ended");
                }
            }
            Err(error) => tracing::debug!(%error, "engine socket unavailable"),
        },
        unsupported => tracing::warn!(?unsupported, "channel not served by this agent"),
    }
}

struct Engine {
    data_root: PathBuf,
    child: Child,
    started: Instant,
    failures: u32,
}

impl Engine {
    fn start(data_root: PathBuf) -> anyhow::Result<Self> {
        let child = spawn_dockerd(&data_root)?;

        Ok(Self {
            data_root,
            child,
            started: Instant::now(),
            failures: 0,
        })
    }

    async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.child.wait().await
    }

    async fn restart(&mut self, status: ExitStatus) -> anyhow::Result<()> {
        if self.started.elapsed() >= STABLE_AFTER {
            self.failures = 0;
        }
        let delay = backoff(self.failures);
        self.failures += 1;
        tracing::warn!(%status, ?delay, "dockerd exited; restarting");

        tokio::time::sleep(delay).await;
        self.child = spawn_dockerd(&self.data_root)?;
        self.started = Instant::now();
        Ok(())
    }

    async fn stop(mut self) {
        if let Some(pid) = self.child.id().and_then(|id| i32::try_from(id).ok()) {
            if let Err(error) = kill(Pid::from_raw(pid), Signal::SIGTERM) {
                tracing::warn!(%error, "cannot signal dockerd");
            }
            if tokio::time::timeout(STOP_GRACE, self.child.wait()).await.is_ok() {
                tracing::info!("dockerd stopped");
                return;
            }
            tracing::warn!("dockerd did not stop in time; killing it");
        }
        if let Err(error) = self.child.kill().await {
            tracing::warn!(%error, "cannot kill dockerd");
        }
    }
}

fn backoff(failures: u32) -> Duration {
    Duration::from_secs(1 << failures.min(5)).min(MAX_BACKOFF)
}

fn spawn_dockerd(data_root: &Path) -> anyhow::Result<Child> {
    fs::create_dir_all(data_root).with_context(|| format!("cannot create {}", data_root.display()))?;
    fs::create_dir_all(LOG_DIR).with_context(|| format!("cannot create {LOG_DIR}"))?;
    let log_path = Path::new(LOG_DIR).join("dockerd.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("cannot open {}", log_path.display()))?;

    let mut command = Command::new(DOCKERD);
    command
        .arg("--data-root")
        .arg(data_root)
        .arg("--host")
        .arg(format!("unix://{}", guest::ENGINE_SOCKET));
    if let Some(gateway) = network::default_gateway() {
        command.arg("--host-gateway-ip").arg(gateway.to_string());
    }
    let child = command
        .env("PATH", SEARCH_PATH)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .context("cannot start dockerd")?;
    tracing::info!(pid = child.id(), data_root = %data_root.display(), "dockerd started");

    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let delays: Vec<_> = (0..8).map(|failures| backoff(failures).as_secs()).collect();

        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30, 30]);
    }
}
