//! Command-line interface to the Contr daemon.

use clap::Parser;
use contr_core::product;

#[derive(Parser)]
#[command(name = product::CLI_BINARY, version, about)]
struct Args {}

fn main() {
    Args::parse();
}
