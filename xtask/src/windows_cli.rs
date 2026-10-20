use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, ensure};

use crate::docker;

/// Exports the Docker CLI tools for Windows that `contrd` installs per user into target/windows-cli.
pub(crate) fn build(root: &Path) -> anyhow::Result<()> {
    docker::ensure_engine_buildx()?;
    let out = root.join("target/windows-cli");
    if out.exists() {
        fs::remove_dir_all(&out).with_context(|| format!("cannot clear {}", out.display()))?;
    }

    let status = Command::new("docker")
        .args(["buildx", "build", "--file"])
        .arg(root.join("packaging/windows/cli.Dockerfile"))
        .arg("--output")
        .arg(format!("type=local,dest={}", out.display()))
        .arg(root.join("packaging/windows"))
        .status()
        .context("cannot run docker buildx")?;
    ensure!(status.success(), "building the Windows CLI tools failed with {status}");

    println!("{}", out.display());
    Ok(())
}
