//! Command-line entry point of the SDK generator.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use contr_sdkgen::Contract;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate SDK sources for one language.
    Generate {
        /// Target language.
        #[arg(long, value_enum)]
        lang: Lang,
        /// Directory holding the proto files and the revision file.
        #[arg(long, default_value = "proto")]
        proto: PathBuf,
        /// Directory that receives the generated sources.
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Lang {
    Rust,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Generate {
            lang: Lang::Rust,
            proto,
            out,
        } => {
            let contract = Contract::load(&proto)?;
            contr_sdkgen::rust::generate(&contract, &out)?;
        }
    }

    Ok(())
}
