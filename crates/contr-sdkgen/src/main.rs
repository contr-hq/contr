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
        /// Template tree of the C# SDK.
        #[arg(long, default_value = "sdk/csharp")]
        template: PathBuf,
        /// Package version; every artifact of a release shares it.
        #[arg(long, default_value = env!("CARGO_PKG_VERSION"))]
        version: String,
    },
    /// Run a generated SDK's conformance program against `contrd --backend fake`.
    Verify {
        /// SDK language; only C# has a conformance program.
        #[arg(long, value_enum)]
        lang: VerifiedLang,
        /// Output directory of `generate`.
        #[arg(long)]
        sdk: PathBuf,
        /// Path of the `contrd` executable.
        #[arg(long)]
        daemon: PathBuf,
        /// Fixture file for the fake backend instead of its built-in set.
        #[arg(long)]
        fixtures: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Lang {
    Rust,
    Csharp,
}

#[derive(Clone, Copy, ValueEnum)]
enum VerifiedLang {
    Csharp,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Generate {
            lang,
            proto,
            out,
            template,
            version,
        } => {
            let contract = Contract::load(&proto)?;
            match lang {
                Lang::Rust => contr_sdkgen::rust::generate(&contract, &out)?,
                Lang::Csharp => contr_sdkgen::csharp::generate(&contract, &template, &out, &version)?,
            }
        }
        Command::Verify {
            lang: VerifiedLang::Csharp,
            sdk,
            daemon,
            fixtures,
        } => contr_sdkgen::verify::csharp(&sdk, &daemon, fixtures.as_deref())?,
    }

    Ok(())
}
