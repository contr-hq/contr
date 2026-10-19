//! Starting the daemon when nothing answers: its systemd user unit on Linux, else the `contrd` executable.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use contr_core::{paths, product};
use contr_transport::Endpoint;
use tokio::process::Command;
use tokio::time::{self, Instant};

use crate::{Client, ClientIdentity, Error};

const START_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) async fn connect_or_start(identity: &ClientIdentity) -> Result<Client, Error> {
    let endpoint = Endpoint::resolve().map_err(Error::Endpoint)?;
    match Client::connect(&endpoint, identity).await {
        Err(Error::Connect { .. }) if std::env::var_os(product::HOST_ENV).is_none() => {}
        result => return result,
    }

    #[cfg(target_os = "linux")]
    start_daemon().await?;
    #[cfg(not(target_os = "linux"))]
    spawn_daemon()?;

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let endpoint = Endpoint::resolve().map_err(Error::Endpoint)?;
        match Client::connect(&endpoint, identity).await {
            Err(Error::Connect { .. }) if Instant::now() < deadline => time::sleep(POLL_INTERVAL).await,
            Err(Error::Connect { endpoint, source }) => {
                return Err(Error::DaemonStart(format!(
                    "nothing answered at {endpoint} within {} seconds: {source}",
                    START_TIMEOUT.as_secs()
                )));
            }
            result => return result,
        }
    }
}

#[cfg(target_os = "linux")]
async fn start_daemon() -> Result<(), Error> {
    match start_user_unit().await {
        Ok(()) => return Ok(()),
        Err(error) => tracing::debug!(%error, "cannot start the daemon's user unit; running the executable"),
    }

    spawn_daemon()
}

#[cfg(target_os = "linux")]
async fn start_user_unit() -> zbus::Result<()> {
    let connection = zbus::Connection::session().await?;
    let manager = zbus::Proxy::new(
        &connection,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .await?;
    let _job: zbus::zvariant::OwnedObjectPath = manager.call("StartUnit", &(product::DAEMON_UNIT, "replace")).await?;

    Ok(())
}

fn spawn_daemon() -> Result<(), Error> {
    let program = daemon_executable();
    let stderr = log_file().map_or_else(|_| Stdio::null(), Stdio::from);

    let mut command = Command::new(&program);
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr);
    detach(&mut command);

    let mut child = command
        .spawn()
        .map_err(|error| Error::DaemonStart(format!("cannot run {}: {error}", program.display())))?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    Ok(())
}

/// `contrd` beside the running executable, else the one on `PATH`.
fn daemon_executable() -> PathBuf {
    let name = format!("{}{}", product::DAEMON_BINARY, std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&name)))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

fn log_file() -> io::Result<File> {
    let dir = paths::runtime_dir().ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    fs::create_dir_all(&dir)?;

    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(dir.join(product::DAEMON_LOG_FILE))
}

/// Keeps the daemon alive when the starting terminal closes or receives Ctrl+C.
#[cfg(unix)]
fn detach(command: &mut Command) {
    command.process_group(0);
}

/// Keeps the daemon alive when the starting console closes or receives Ctrl+C.
#[cfg(windows)]
fn detach(command: &mut Command) {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}
