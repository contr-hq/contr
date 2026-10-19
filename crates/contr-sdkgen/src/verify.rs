//! SDK conformance: runs a generated SDK's conformance program against `contrd --backend fake`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use contr_core::product;

use crate::Error;
use crate::csharp::VERIFY_PROJECT;

const LISTEN_TIMEOUT: Duration = Duration::from_secs(20);
const LISTEN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Starts the fake daemon on a private endpoint and runs the C# conformance program in `sdk_dir` against it.
pub fn csharp(sdk_dir: &Path, daemon: &Path, fixtures: Option<&Path>) -> Result<(), Error> {
    let endpoint = PrivateEndpoint::new()?;
    let mut command = Command::new(daemon);
    command
        .args(["--backend", "fake", "--no-docker-context", "--listen"])
        .arg(endpoint.uri());
    if let Some(fixtures) = fixtures {
        command.arg("--fixtures").arg(fixtures);
    }
    let daemon_process = KillOnDrop(command.spawn().map_err(|source| Error::Spawn {
        program: daemon.display().to_string(),
        source,
    })?);
    endpoint.wait_until_listening()?;

    let project = sdk_dir.join(VERIFY_PROJECT);
    let status = Command::new("dotnet")
        .args(["run", "--configuration", "Release", "--project"])
        .arg(&project)
        .env(product::HOST_ENV, endpoint.uri())
        .status()
        .map_err(|source| Error::Spawn {
            program: "dotnet".to_owned(),
            source,
        })?;
    drop(daemon_process);

    if status.success() {
        Ok(())
    } else {
        Err(Error::Failed {
            program: format!("dotnet run --project {}", project.display()),
            status,
        })
    }
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Endpoint no other daemon uses: a socket in a fresh directory, or a pipe named after this process.
struct PrivateEndpoint {
    path: PathBuf,
    #[cfg(unix)]
    _directory: tempfile::TempDir,
}

impl PrivateEndpoint {
    #[cfg(unix)]
    fn new() -> Result<Self, Error> {
        let directory = tempfile::Builder::new()
            .prefix("contr-sdkgen-")
            .tempdir()
            .map_err(|source| Error::Write {
                path: std::env::temp_dir(),
                source,
            })?;

        Ok(Self {
            path: directory.path().join("run").join(product::API_SOCKET_FILE),
            _directory: directory,
        })
    }

    #[cfg(windows)]
    #[allow(clippy::unnecessary_wraps, reason = "creating the Unix endpoint can fail")]
    fn new() -> Result<Self, Error> {
        Ok(Self {
            path: PathBuf::from(format!(r"\\.\pipe\{}-sdkgen-{}", product::SLUG, std::process::id())),
        })
    }

    #[cfg(unix)]
    fn uri(&self) -> String {
        format!("unix://{}", self.path.display())
    }

    #[cfg(windows)]
    fn uri(&self) -> String {
        format!("npipe://{}", self.path.display().to_string().replace('\\', "/"))
    }

    fn wait_until_listening(&self) -> Result<(), Error> {
        let deadline = Instant::now() + LISTEN_TIMEOUT;
        while !self.path.exists() {
            if Instant::now() >= deadline {
                return Err(Error::DaemonStart(self.uri()));
            }
            thread::sleep(LISTEN_POLL_INTERVAL);
        }

        Ok(())
    }
}
