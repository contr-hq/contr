use std::time::Duration;

use clap::Subcommand;
use contr_sdk::api::engine::v1::EngineState;
use contr_sdk::api::integration::v1::{
    ListWslDistributionsRequest, SetWslIntegrationRequest, WslDistribution, WslIntegrationState,
};
use contr_sdk::{Client, Error};
use tokio::time::{Instant, sleep};

use crate::format;

/// How long `enable` waits for a running engine to reach the distribution.
const SETTLE_WAIT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Subcommand)]
pub(crate) enum WslCommand {
    /// List your WSL distributions and whether each reaches the engine.
    List,
    /// Let a distribution reach the engine through `docker` and /var/run/docker.sock.
    Enable { distribution: String },
    /// Remove the engine from a distribution.
    Disable { distribution: String },
}

pub(crate) async fn run(client: &Client, command: WslCommand) -> anyhow::Result<i32> {
    match command {
        WslCommand::List => {
            print(&list(client).await?);
            Ok(0)
        }
        WslCommand::Enable { distribution } => set(client, distribution, true).await,
        WslCommand::Disable { distribution } => set(client, distribution, false).await,
    }
}

async fn list(client: &Client) -> Result<Vec<WslDistribution>, Error> {
    Ok(client
        .integration()
        .list_wsl_distributions(ListWslDistributionsRequest {})
        .await?
        .into_inner()
        .distributions)
}

async fn set(client: &Client, name: String, enabled: bool) -> anyhow::Result<i32> {
    let mut distribution = client
        .integration()
        .set_wsl_integration(SetWslIntegrationRequest { name, enabled })
        .await
        .map_err(Error::from)?
        .into_inner()
        .distribution
        .unwrap_or_default();

    let pending = distribution.integration_state() == WslIntegrationState::Pending;
    if pending && client.engine_state().await?.state() == EngineState::Running {
        distribution = settle(client, distribution).await?;
    }
    print(std::slice::from_ref(&distribution));

    Ok(i32::from(
        distribution.integration_state() == WslIntegrationState::Failed,
    ))
}

/// The distribution once its integration leaves PENDING, or as it is when that takes too long.
async fn settle(client: &Client, mut distribution: WslDistribution) -> Result<WslDistribution, Error> {
    let deadline = Instant::now() + SETTLE_WAIT;
    while distribution.integration_state() == WslIntegrationState::Pending && Instant::now() < deadline {
        sleep(POLL_INTERVAL).await;
        if let Some(current) = list(client).await?.into_iter().find(|d| d.name == distribution.name) {
            distribution = current;
        }
    }

    Ok(distribution)
}

fn print(distributions: &[WslDistribution]) {
    let rows: Vec<Vec<String>> = distributions
        .iter()
        .map(|distribution| {
            let marker = if distribution.is_default { " *" } else { "" };
            vec![
                format!("{}{marker}", distribution.name),
                distribution.wsl_version.to_string(),
                integration(distribution),
            ]
        })
        .collect();

    print!("{}", format::table(&["NAME", "WSL", "INTEGRATION"], &rows));
}

fn integration(distribution: &WslDistribution) -> String {
    match distribution.integration_state() {
        WslIntegrationState::Active => "active".to_owned(),
        WslIntegrationState::Pending => "pending (starts with the engine)".to_owned(),
        WslIntegrationState::Failed => format!("failed: {}", distribution.failure),
        WslIntegrationState::Disabled | WslIntegrationState::Unspecified => {
            if distribution.wsl_version < 2 {
                "unavailable on WSL 1".to_owned()
            } else {
                "off".to_owned()
            }
        }
    }
}
