//! Privileged Linux helper run through pkexec for an allow-listed set of operations.

use clap::Parser;
use contr_core::product;

#[derive(Parser)]
#[command(name = product::HELPER_BINARY, version, about)]
struct Args {}

fn main() {
    Args::parse();
}
