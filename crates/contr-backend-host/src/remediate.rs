//! Fixes for an engine the user cannot use: installing one, joining its group, or setting up a rootless one.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use contr_backend::{BackendError, Progress, ProgressSink, RemediationOutcome};
use contr_core::product;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Docker's tool that sets up a rootless engine for the user who runs it.
const ROOTLESS_SETUP_TOOL: &str = "dockerd-rootless-setuptool.sh";
/// pkexec's exit code when the user dismissed its password prompt.
const PKEXEC_DISMISSED: i32 = 126;
/// Lines of a failed command's standard error kept for its error message.
const KEPT_ERROR_LINES: usize = 20;

/// Installs the distribution's engine packages, starts the engine and adds the user to its group, behind one
/// password prompt; the group applies from the user's next login.
pub(crate) async fn install_engine(progress: &dyn ProgressSink) -> Result<RemediationOutcome, BackendError> {
    run_helper("install-engine", progress).await?;
    Ok(RemediationOutcome {
        restart_required: false,
        logout_required: true,
    })
}

/// Adds the user to the engine's group behind a password prompt; it applies from the user's next login.
pub(crate) async fn join_docker_group(progress: &dyn ProgressSink) -> Result<RemediationOutcome, BackendError> {
    run_helper("join-docker-group", progress).await?;
    Ok(RemediationOutcome {
        restart_required: false,
        logout_required: true,
    })
}

/// Sets up a rootless engine for the user with Docker's own tool, which needs no password.
pub(crate) async fn setup_rootless(progress: &dyn ProgressSink) -> Result<RemediationOutcome, BackendError> {
    let tool = on_path(ROOTLESS_SETUP_TOOL).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("rootless Docker needs {ROOTLESS_SETUP_TOOL}, which Docker's rootless extras package installs"),
        )
    })?;
    let mut command = Command::new(tool);
    command.arg("install");
    stream(command, progress).await?;

    Ok(RemediationOutcome::default())
}

async fn run_helper(operation: &str, progress: &dyn ProgressSink) -> Result<(), BackendError> {
    let helper = helper()?;
    let mut command = Command::new("pkexec");
    command.arg(helper).arg(operation);

    match stream(command, progress).await {
        Err(Failure::Exited(PKEXEC_DISMISSED, _)) => Err(BackendError::Dismissed),
        Err(failure) => Err(failure.into()),
        Ok(()) => Ok(()),
    }
}

/// The helper beside this executable, else the one packages install.
fn helper() -> io::Result<PathBuf> {
    let beside = std::env::current_exe()?
        .parent()
        .map(|dir| dir.join(product::HELPER_BINARY))
        .filter(|path| path.is_file());

    beside
        .or_else(|| Some(PathBuf::from(product::INSTALLED_HELPER)).filter(|path| path.is_file()))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not installed", product::INSTALLED_HELPER),
            )
        })
}

fn on_path(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
}

#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error("{} failed: {}", .1.0, .1.1)]
    Exited(i32, (String, String)),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<Failure> for BackendError {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Io(error) => error.into(),
            exited @ Failure::Exited(..) => io::Error::other(exited.to_string()).into(),
        }
    }
}

/// Runs `command`, reporting each line of its standard output as progress, and fails with the error its standard
/// error ends with.
async fn stream(mut command: Command, progress: &dyn ProgressSink) -> Result<(), Failure> {
    let program = Path::new(command.as_std().get_program())
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let stdout = child.stdout.take().map(|out| BufReader::new(out).lines());
    let stderr = child.stderr.take().map(|err| BufReader::new(err).lines());
    let report = async {
        if let Some(mut lines) = stdout {
            while let Some(line) = lines.next_line().await? {
                progress.report(Progress {
                    message: line,
                    fraction: None,
                });
            }
        }
        Ok::<(), io::Error>(())
    };
    let keep = async {
        let mut kept = VecDeque::with_capacity(KEPT_ERROR_LINES);
        if let Some(mut lines) = stderr {
            while let Some(line) = lines.next_line().await? {
                if kept.len() == KEPT_ERROR_LINES {
                    kept.pop_front();
                }
                kept.push_back(line);
            }
        }
        Ok::<VecDeque<String>, io::Error>(kept)
    };
    let (reported, kept) = tokio::join!(report, keep);
    reported?;
    let kept = kept?;
    let status = child.wait().await?;
    if status.success() {
        return Ok(());
    }

    let reason = kept
        .iter()
        .rev()
        .find_map(|line| line.strip_prefix("Error: "))
        .or_else(|| kept.back().map(String::as_str))
        .unwrap_or("no reason given")
        .to_owned();
    Err(Failure::Exited(status.code().unwrap_or(-1), (program, reason)))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Steps(Mutex<Vec<String>>);

    impl ProgressSink for Steps {
        fn report(&self, progress: Progress) {
            if let Ok(mut steps) = self.0.lock() {
                steps.push(progress.message);
            }
        }
    }

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn the_polkit_policy_names_the_action_and_the_installed_helper() {
        let policy = include_str!("../../../packaging/linux/polkit/io.github.contr_hq.contr.policy");

        assert!(policy.contains(&format!("<action id=\"{}\">", product::POLKIT_MANAGE_ENGINE)));
        assert!(policy.contains(&format!(">{}</annotate>", product::INSTALLED_HELPER)));
    }

    #[tokio::test]
    async fn reports_each_step_and_succeeds() {
        let steps = Steps::default();

        stream(shell("echo 'Installing a'; echo noise >&2; echo 'Starting b'"), &steps)
            .await
            .unwrap();

        assert_eq!(*steps.0.lock().unwrap(), ["Installing a", "Starting b"]);
    }

    #[tokio::test]
    async fn fails_with_the_error_the_command_printed() {
        let steps = Steps::default();

        let failure = stream(
            shell("echo 'Installing a'; echo 'E: Unable to locate package' >&2; echo 'Error: apt-get install failed' >&2; exit 1"),
            &steps,
        )
        .await
        .unwrap_err();

        assert!(matches!(failure, Failure::Exited(1, _)));
        assert_eq!(failure.to_string(), "sh failed: apt-get install failed");
    }
}
