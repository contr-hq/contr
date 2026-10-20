//! Repository automation, run as `cargo xtask <task>`.

mod docker;
mod guest;
mod windows_cli;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use contr_sdkgen::Contract;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Regenerate crates/contr-api/src/gen from proto/.
    Codegen,
    /// Build the runtime and data guest root filesystems into target/guest with `docker buildx`.
    Guest {
        /// Docker platform architecture of the guests.
        #[arg(long, default_value = "amd64", value_parser = ["amd64", "arm64"])]
        arch: String,
    },
    /// Fetch the Docker CLI tools for Windows that contrd installs per user into target/windows-cli.
    WindowsCli,
}

fn main() -> anyhow::Result<()> {
    let root = workspace_root();

    match Cli::parse().task {
        Task::Codegen => {
            let contract = Contract::load(&root.join("proto"))?;
            contr_sdkgen::rust::generate(&contract, &root.join("crates/contr-api/src/gen"))?;
        }
        Task::Guest { arch } => guest::build(&root, &arch)?,
        Task::WindowsCli => windows_cli::build(&root)?,
    }

    Ok(())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}
