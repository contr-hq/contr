//! `contr exec`: an exec session relayed over the daemon's gRPC stream, with a raw local terminal for `-t`.

use std::io::{IsTerminal, Write};

use clap::Args;
use contr_sdk::api::containers::v1::{ExecStart, TerminalSize};
use contr_sdk::{Client, ExecEvent, ExecInput};
use tokio::io::AsyncReadExt;

#[derive(Args)]
pub(crate) struct ExecArgs {
    /// Keep stdin open and forward it.
    #[arg(short, long)]
    interactive: bool,
    /// Allocate a pseudo-terminal.
    #[arg(short, long)]
    tty: bool,
    /// Set an environment variable, as NAME=VALUE.
    #[arg(short, long = "env", value_name = "NAME=VALUE")]
    env: Vec<String>,
    /// User to run as, as name or UID[:GID].
    #[arg(short, long)]
    user: Option<String>,
    /// Working directory inside the container.
    #[arg(short, long)]
    workdir: Option<String>,
    /// Container ID or name.
    container: String,
    /// Command and its arguments.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

/// Runs the command and returns its exit code.
pub(crate) async fn run(client: &Client, args: ExecArgs) -> anyhow::Result<i32> {
    let terminal_size = args.tty.then(terminal_size).flatten();
    let start = ExecStart {
        container_id: args.container,
        command: args.command,
        tty: args.tty,
        attach_stdin: args.interactive,
        env: args.env,
        user: args.user.unwrap_or_default(),
        working_dir: args.workdir.unwrap_or_default(),
        terminal_size,
        privileged: false,
    };

    let mut session = client.exec(start).await?;
    let _raw_mode = (args.tty && args.interactive && std::io::stdin().is_terminal())
        .then(RawMode::enable)
        .transpose()?;

    if args.interactive {
        tokio::spawn(forward_stdin(session.input()));
    }
    if args.tty {
        tokio::spawn(forward_resizes(session.input()));
    }

    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    while let Some(event) = session.next_event().await? {
        match event {
            ExecEvent::Stdout(bytes) => {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            ExecEvent::Stderr(bytes) => {
                stderr.write_all(&bytes)?;
                stderr.flush()?;
            }
            ExecEvent::Exited(code) => return Ok(code),
        }
    }

    anyhow::bail!("the exec ended without an exit code")
}

fn terminal_size() -> Option<TerminalSize> {
    let (columns, rows) = crossterm::terminal::size().ok()?;
    Some(TerminalSize {
        rows: rows.into(),
        columns: columns.into(),
    })
}

/// Raw mode for the local terminal; restored on drop.
struct RawMode;

impl RawMode {
    fn enable() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

async fn forward_stdin(input: ExecInput) {
    let mut stdin = tokio::io::stdin();
    let mut buffer = vec![0; 4096];

    loop {
        match stdin.read(&mut buffer).await {
            Ok(0) | Err(_) => {
                let _ = input.close().await;
                return;
            }
            Ok(read) => {
                if input.write(&buffer[..read]).await.is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(unix)]
async fn forward_resizes(input: ExecInput) {
    use tokio::signal::unix::{SignalKind, signal};

    let Ok(mut changes) = signal(SignalKind::window_change()) else {
        return;
    };
    loop {
        if let Ok((columns, rows)) = crossterm::terminal::size()
            && input.resize(rows, columns).await.is_err()
        {
            return;
        }
        if changes.recv().await.is_none() {
            return;
        }
    }
}

/// Windows consoles send no resize signal, so the size is polled.
#[cfg(windows)]
async fn forward_resizes(input: ExecInput) {
    let mut last = None;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(250));

    loop {
        ticker.tick().await;
        let size = crossterm::terminal::size().ok();
        if size != last {
            last = size;
            if let Some((columns, rows)) = size
                && input.resize(rows, columns).await.is_err()
            {
                return;
            }
        }
    }
}
