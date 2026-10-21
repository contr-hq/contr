use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use contr_core::product;
use contr_sdk::api::diagnostics::v1::run_remediation_response::Event;
use contr_sdk::api::diagnostics::v1::{
    Check, CheckStatus, CreateBundleRequest, RunChecksRequest, RunRemediationRequest, create_bundle_response,
};
use contr_sdk::{Client, Error};

use crate::format;

/// Prints every check and the fixes on offer; exits 1 when a check fails.
pub(crate) async fn doctor(client: &Client) -> anyhow::Result<i32> {
    let checks = client
        .diagnostics()
        .run_checks(RunChecksRequest {})
        .await
        .map_err(Error::from)?
        .into_inner()
        .checks;

    let rows: Vec<Vec<String>> = checks
        .iter()
        .map(|check| {
            vec![
                status_name(check.status()).to_owned(),
                check.title.clone(),
                check.detail.clone(),
            ]
        })
        .collect();
    print!("{}", format::table(&["STATUS", "CHECK", "DETAIL"], &rows));
    for remediation in checks.iter().flat_map(|check| &check.remediations) {
        let elevation = if remediation.requires_elevation {
            " (asks for administrator rights)"
        } else {
            ""
        };
        println!("Fix: {} fix {}{elevation}", product::CLI_BINARY, remediation.action_id);
    }

    Ok(i32::from(checks.iter().any(failed)))
}

/// Runs one remediation and reports what the user must still do.
pub(crate) async fn fix(client: &Client, action_id: String) -> anyhow::Result<i32> {
    let mut events = client
        .diagnostics()
        .run_remediation(RunRemediationRequest { action_id })
        .await
        .map_err(Error::from)?
        .into_inner();
    while let Some(event) = events.message().await.map_err(Error::from)? {
        match event.event {
            Some(Event::Progress(progress)) => eprintln!("{}", progress.message),
            Some(Event::Outcome(outcome)) if outcome.restart_required => {
                println!("Done. Restart the computer to finish.");
            }
            Some(Event::Outcome(outcome)) if outcome.logout_required => {
                println!("Done. Sign out and back in to finish.");
            }
            Some(Event::Outcome(_)) => println!("Done."),
            None => {}
        }
    }

    Ok(0)
}

/// Writes a diagnostics bundle to `output`, or to a new file in the current directory.
pub(crate) async fn bundle(client: &Client, output: Option<PathBuf>) -> anyhow::Result<i32> {
    let mut events = client
        .diagnostics()
        .create_bundle(CreateBundleRequest {})
        .await
        .map_err(Error::from)?
        .into_inner();
    let mut zip = Vec::new();
    while let Some(event) = events.message().await.map_err(Error::from)? {
        match event.event {
            Some(create_bundle_response::Event::Progress(progress)) => eprintln!("{}", progress.message),
            Some(create_bundle_response::Event::Chunk(chunk)) => zip.extend_from_slice(&chunk),
            None => {}
        }
    }

    let path = output.unwrap_or_else(|| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default();
        PathBuf::from(format!("{}-diagnostics-{now}.zip", product::SLUG))
    });
    std::fs::write(&path, &zip).with_context(|| format!("cannot write {}", path.display()))?;
    println!(
        "Saved {}. Credentials, your name, home directory and computer name are replaced; look through it before \
         you share it.",
        path.display()
    );

    Ok(0)
}

fn failed(check: &Check) -> bool {
    check.status() == CheckStatus::Fail
}

fn status_name(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Pass => "ok",
        CheckStatus::Warn => "warning",
        CheckStatus::Fail => "failed",
        CheckStatus::Skip | CheckStatus::Unspecified => "skipped",
    }
}
