//! What the engine's systemd unit logged recently, for diagnostics bundles.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::discovery::Bus;

const LINES: &str = "300";
const TIMEOUT: Duration = Duration::from_secs(10);

/// The last lines `unit` logged, or why journalctl could not read them; reading the system journal needs the
/// `systemd-journal` or `adm` group.
pub(crate) async fn recent(bus: Bus, unit: &str) -> String {
    let mut command = Command::new("journalctl");
    if matches!(bus, Bus::Session) {
        command.arg("--user");
    }
    command
        .args(["--unit", unit, "--lines", LINES, "--no-pager", "--output", "short-iso"])
        .stdin(Stdio::null())
        .kill_on_drop(true);

    match tokio::time::timeout(TIMEOUT, command.output()).await {
        Ok(Ok(output)) => format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        Ok(Err(error)) => format!("cannot run journalctl: {error}\n"),
        Err(_) => format!("journalctl did not answer within {} seconds\n", TIMEOUT.as_secs()),
    }
}
