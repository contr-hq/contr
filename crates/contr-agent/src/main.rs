//! Guest agent that supervises the engine inside the Contr WSL distribution or virtual machine. Standard output
//! carries the protocol, so every log line goes to standard error.

#[cfg(target_os = "linux")]
mod configure;
#[cfg(target_os = "linux")]
mod data;
#[cfg(target_os = "linux")]
mod engine;
#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "linux")]
mod integrate;
#[cfg(target_os = "linux")]
mod mounts;
#[cfg(target_os = "linux")]
mod share;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use contr_core::product;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = product::AGENT_BINARY, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine and serve multiplexed streams on standard input and output until the host closes them.
    Serve {
        /// Directory that holds the engine's data; defaults to the data guest's mount.
        #[arg(long)]
        data_root: Option<PathBuf>,
        /// Also serve the engine to the user's other WSL distributions through the shared engine socket.
        #[arg(long)]
        share: bool,
    },
    /// Write the engine configuration on standard input for the engine's next start.
    Configure,
    /// Mount the data guest's storage for the runtime guest and hold it until standard input closes.
    DataAnchor,
    /// Serve the engine to the WSL distribution this process runs in until standard input closes.
    Integrate {
        /// Name of this distribution, as `wsl.exe` lists it.
        #[arg(long)]
        distribution: String,
        /// User ID that owns the distribution's Docker socket.
        #[arg(long)]
        owner: u32,
    },
    /// Unmount the root file system that an integration of `distribution` left mounted.
    ReleaseDistribution { distribution: String },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env(product::LOG_ENV).unwrap_or_else(|_| EnvFilter::new("info")))
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();

    run(cli.command)
}

#[cfg(target_os = "linux")]
fn run(command: Command) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    match command {
        Command::Serve { data_root, share } => runtime.block_on(engine::serve(data_root, share)),
        Command::Configure => configure::configure(),
        Command::DataAnchor => runtime.block_on(data::anchor()),
        Command::Integrate { distribution, owner } => runtime.block_on(integrate::integrate(&distribution, owner)),
        Command::ReleaseDistribution { distribution } => share::release_distributions(Some(&distribution)),
    }
}

#[cfg(not(target_os = "linux"))]
fn run(_command: Command) -> anyhow::Result<()> {
    anyhow::bail!("{} runs only inside Linux guests", product::AGENT_BINARY)
}
