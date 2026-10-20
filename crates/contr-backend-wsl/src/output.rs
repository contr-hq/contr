//! Output of the processes `contrd` runs in WSL distributions.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader, Lines};
use tokio::task::JoinHandle;

/// The latest line a process wrote to standard error, which explains why it failed.
pub(crate) type LatestLine = Arc<Mutex<String>>;

/// Logs each standard error line of a process in `distribution`, keeping the latest in `latest` when given.
pub(crate) fn forward_log<R>(stderr: Option<R>, distribution: &str, latest: Option<LatestLine>) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let distribution = distribution.to_owned();
    tokio::spawn(async move {
        let Some(stderr) = stderr else {
            return;
        };
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(target: "contr_agent", %distribution, "{line}");
            if let Some(latest) = &latest {
                *latest.lock().unwrap_or_else(PoisonError::into_inner) = line;
            }
        }
    })
}

/// Reads lines until one equals `expected`; `false` when the output ends first.
pub(crate) async fn wait_for_line<R>(lines: &mut Lines<BufReader<R>>, expected: &str) -> io::Result<bool>
where
    R: AsyncRead + Unpin,
{
    while let Some(line) = lines.next_line().await? {
        if line.trim() == expected {
            return Ok(true);
        }
    }

    Ok(false)
}
