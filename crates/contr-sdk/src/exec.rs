use contr_api::containers::v1::containers_service_client::ContainersServiceClient;
use contr_api::containers::v1::exec_request::Input;
use contr_api::containers::v1::exec_response::Output;
use contr_api::containers::v1::{ExecCloseStdin, ExecRequest, ExecResponse, ExecStart, TerminalSize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Streaming;
use tonic::transport::Channel;

use crate::Error;

const INPUT_QUEUE: usize = 64;

/// Output of an exec session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecEvent {
    /// Standard output, or all output of a command with a terminal.
    Stdout(Vec<u8>),
    /// Standard error.
    Stderr(Vec<u8>),
    /// The command exited with this code; no event follows.
    Exited(i32),
}

/// Command running in a container through the daemon's `Exec` stream. Dropping the session and every [`ExecInput`]
/// ends the request stream, which closes the command's stdin.
pub struct ExecSession {
    id: String,
    input: ExecInput,
    responses: Streaming<ExecResponse>,
}

impl ExecSession {
    pub(crate) async fn start(mut client: ContainersServiceClient<Channel>, start: ExecStart) -> Result<Self, Error> {
        let (sender, receiver) = mpsc::channel(INPUT_QUEUE);
        let input = ExecInput(sender);
        input.send(Input::Start(start)).await?;

        let mut responses = client.exec(ReceiverStream::new(receiver)).await?.into_inner();
        let Some(ExecResponse {
            output: Some(Output::Started(started)),
        }) = responses.message().await?
        else {
            return Err(Error::Protocol("the first exec response must be started"));
        };

        Ok(Self {
            id: started.exec_id,
            input,
            responses,
        })
    }

    /// Engine ID of the exec.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Handle that sends stdin and terminal sizes; clones may live on other tasks.
    pub fn input(&self) -> ExecInput {
        self.input.clone()
    }

    /// Next output event; `None` after [`ExecEvent::Exited`].
    pub async fn next_event(&mut self) -> Result<Option<ExecEvent>, Error> {
        let Some(response) = self.responses.message().await? else {
            return Ok(None);
        };

        match response.output {
            Some(Output::Stdout(bytes)) => Ok(Some(ExecEvent::Stdout(bytes))),
            Some(Output::Stderr(bytes)) => Ok(Some(ExecEvent::Stderr(bytes))),
            Some(Output::Exited(exited)) => Ok(Some(ExecEvent::Exited(exited.exit_code))),
            Some(Output::Started(_)) | None => Err(Error::Protocol("unexpected exec response")),
        }
    }
}

/// Input side of an [`ExecSession`].
#[derive(Clone, Debug)]
pub struct ExecInput(mpsc::Sender<ExecRequest>);

impl ExecInput {
    /// Sends bytes to the command's stdin; ignored unless the exec attached stdin.
    pub async fn write(&self, bytes: impl Into<Vec<u8>>) -> Result<(), Error> {
        self.send(Input::Stdin(bytes.into())).await
    }

    /// Resizes the command's pseudo-terminal.
    pub async fn resize(&self, rows: u16, columns: u16) -> Result<(), Error> {
        self.send(Input::Resize(TerminalSize {
            rows: rows.into(),
            columns: columns.into(),
        }))
        .await
    }

    /// Closes the command's stdin.
    pub async fn close(&self) -> Result<(), Error> {
        self.send(Input::CloseStdin(ExecCloseStdin {})).await
    }

    async fn send(&self, input: Input) -> Result<(), Error> {
        self.0
            .send(ExecRequest { input: Some(input) })
            .await
            .map_err(|_| Error::Closed)
    }
}
