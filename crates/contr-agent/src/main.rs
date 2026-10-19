//! Guest agent that supervises the engine inside the Contr WSL distribution or virtual machine.

use clap::Parser;
use contr_core::product;

#[derive(Parser)]
#[command(name = product::AGENT_BINARY, version, about)]
struct Args {}

fn main() {
    Args::parse();
}
