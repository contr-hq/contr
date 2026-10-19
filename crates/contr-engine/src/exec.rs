use std::pin::Pin;

use bollard::container::LogOutput;
use bollard::exec::{ResizeExecOptions, StartExecOptions, StartExecResults};
use bollard::models::ExecConfig;
use bytes::Bytes;
use contr_api::containers::v1::{ExecStart, TerminalSize};
use futures_util::stream::BoxStream;
use futures_util::{Stream, StreamExt};
use tokio::io::AsyncWrite;

use crate::{Engine, EngineError};

/// Output of a running exec; a terminal's combined output arrives as [`ExecOutput::Stdout`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecOutput {
    Stdout(Bytes),
    Stderr(Bytes),
}

/// Command started in a container, attached through a hijacked engine connection.
pub struct ExecSession {
    pub id: String,
    pub output: BoxStream<'static, Result<ExecOutput, EngineError>>,
    /// Shutting it down closes the command's stdin.
    pub input: Pin<Box<dyn AsyncWrite + Send>>,
}

impl Engine {
    /// Creates and starts the command `start` describes.
    pub async fn exec(&self, start: &ExecStart) -> Result<ExecSession, EngineError> {
        let console_size = start
            .terminal_size
            .filter(|size| start.tty && size.rows > 0 && size.columns > 0)
            .map(|size| vec![size.rows as usize, size.columns as usize]);
        let config = ExecConfig {
            attach_stdin: Some(start.attach_stdin),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            console_size,
            tty: Some(start.tty),
            env: Some(start.env.clone()).filter(|env| !env.is_empty()),
            cmd: Some(start.command.clone()),
            privileged: Some(start.privileged),
            user: Some(start.user.clone()).filter(|user| !user.is_empty()),
            working_dir: Some(start.working_dir.clone()).filter(|dir| !dir.is_empty()),
            detach_keys: None,
        };

        let id = self.docker().create_exec(&start.container_id, config).await?.id;
        let options = StartExecOptions {
            detach: false,
            tty: start.tty,
            output_capacity: None,
        };

        match self.docker().start_exec(&id, Some(options)).await? {
            StartExecResults::Attached { output, input } => Ok(ExecSession {
                id,
                output: exec_output(output).boxed(),
                input,
            }),
            StartExecResults::Detached => Err(EngineError::Engine("the engine detached the exec".to_owned())),
        }
    }

    pub async fn resize_exec(&self, id: &str, size: TerminalSize) -> Result<(), EngineError> {
        let options = ResizeExecOptions {
            height: u16::try_from(size.rows).unwrap_or(u16::MAX),
            width: u16::try_from(size.columns).unwrap_or(u16::MAX),
        };
        Ok(self.docker().resize_exec(id, options).await?)
    }

    /// Exit code of a finished exec.
    pub async fn exec_exit_code(&self, id: &str) -> Result<i32, EngineError> {
        let inspect = self.docker().inspect_exec(id).await?;
        let code = inspect.exit_code.unwrap_or_default();

        Ok(i32::try_from(code).unwrap_or(i32::MIN))
    }
}

fn exec_output(
    output: impl Stream<Item = Result<LogOutput, bollard::errors::Error>> + Send + 'static,
) -> impl Stream<Item = Result<ExecOutput, EngineError>> + Send + 'static {
    output.filter_map(|output| {
        std::future::ready(match output {
            Ok(LogOutput::StdOut { message } | LogOutput::Console { message }) => Some(Ok(ExecOutput::Stdout(message))),
            Ok(LogOutput::StdErr { message }) => Some(Ok(ExecOutput::Stderr(message))),
            Ok(LogOutput::StdIn { .. }) => None,
            Err(error) => Some(Err(error.into())),
        })
    })
}
