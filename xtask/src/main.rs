//! Repository automation, run as `cargo xtask <task>`.

mod dist;
mod docker;
mod guest;
mod guest_sources;
mod notices;
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
    /// Fetch the exact source of every Alpine package in the images in target/guest into target/guest-sources.
    GuestSources,
    /// Assemble the Windows core payload the client installer carries from Windows builds of contrd and contr,
    /// target/guest and target/windows-cli.
    DistWindows {
        /// Directory holding contrd.exe and contr.exe built for Windows.
        #[arg(long)]
        binaries: PathBuf,
        /// Directory that receives the payload.
        #[arg(long, default_value = "target/dist")]
        out: PathBuf,
    },
    /// Assemble the Linux archive from Linux builds of contrd and contr.
    DistLinux {
        /// Directory holding contrd and contr built for Linux.
        #[arg(long)]
        binaries: PathBuf,
        /// Directory that receives the archive.
        #[arg(long, default_value = "target/dist")]
        out: PathBuf,
    },
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
        Task::GuestSources => guest_sources::fetch(&root)?,
        Task::DistWindows { binaries, out } => dist::windows(&root, &binaries, &out)?,
        Task::DistLinux { binaries, out } => dist::linux(&root, &binaries, &out)?,
    }

    Ok(())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}
