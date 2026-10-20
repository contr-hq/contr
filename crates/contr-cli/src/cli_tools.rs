use clap::Subcommand;
use contr_core::product;
use contr_sdk::api::integration::v1::{CliTools, GetCliToolsRequest, InstallCliToolsRequest};
use contr_sdk::{Client, Error};

use crate::format;

#[derive(Subcommand)]
pub(crate) enum CliCommand {
    /// Show the bundled and installed tools and which `docker` a new shell runs.
    Status,
    /// Install the bundled tools for this user and add them to the user's PATH.
    Install,
}

pub(crate) async fn run(client: &Client, command: CliCommand) -> anyhow::Result<i32> {
    let tools = match command {
        CliCommand::Status => {
            client
                .integration()
                .get_cli_tools(GetCliToolsRequest {})
                .await
                .map_err(Error::from)?
                .into_inner()
                .cli_tools
        }
        CliCommand::Install => {
            client
                .integration()
                .install_cli_tools(InstallCliToolsRequest {})
                .await
                .map_err(Error::from)?
                .into_inner()
                .cli_tools
        }
    }
    .unwrap_or_default();

    print_tools(&tools);
    Ok(i32::from(!tools.on_path))
}

fn print_tools(tools: &CliTools) {
    if tools.tools.is_empty() {
        println!("This daemon bundles no Docker CLI tools.");
        return;
    }

    let missing = || "-".to_owned();
    let rows: Vec<Vec<String>> = tools
        .tools
        .iter()
        .map(|tool| {
            vec![
                tool.name.clone(),
                Some(tool.bundled_version.clone())
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(missing),
                Some(tool.installed_version.clone())
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(missing),
            ]
        })
        .collect();
    print!("{}", format::table(&["TOOL", "BUNDLED", "INSTALLED"], &rows));

    println!("Location:  {}", tools.bin_dir);
    if tools.on_path {
        println!("PATH:      new shells run this docker");
    } else if !tools.shadowed_by.is_empty() {
        println!("PATH:      new shells run {} first", tools.shadowed_by);
    } else {
        println!("PATH:      not on PATH; run `{} cli install`", product::CLI_BINARY);
    }
}
