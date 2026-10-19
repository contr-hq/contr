//! Repository automation, run as `cargo xtask <task>`.

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
}

fn main() -> anyhow::Result<()> {
    let root = workspace_root();

    match Cli::parse().task {
        Task::Codegen => {
            let contract = Contract::load(&root.join("proto"))?;
            contr_sdkgen::rust::generate(&contract, &root.join("crates/contr-api/src/gen"))?;
        }
    }

    Ok(())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}
