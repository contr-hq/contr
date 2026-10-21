//! Compose projects: the engine's containers grouped by the labels Compose sets, what a project's Compose files
//! declare, and Compose run against the engine.

mod project;
mod runner;

pub use project::{Added, Declared, DeclaredService, added_name, config_files, group, labels};
pub use runner::{Compose, ComposeError, Output, Target};
