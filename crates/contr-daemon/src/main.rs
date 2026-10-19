//! The Contr daemon, which manages the container engine and serves the Contr gRPC API.

use clap::Parser;
use contr_core::product;

#[derive(Parser)]
#[command(name = product::DAEMON_BINARY, version, about)]
struct Args {}

fn main() {
    Args::parse();
}
