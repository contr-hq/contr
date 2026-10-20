use std::process::Command;

use anyhow::{Context, ensure};

/// Refuses Docker Desktop's buildx plugin, which shadows the engine's own plugin where both are installed;
/// see CLAUDE.md.
pub(crate) fn ensure_engine_buildx() -> anyhow::Result<()> {
    let output = Command::new("docker")
        .args(["buildx", "version"])
        .output()
        .context("cannot run docker buildx")?;
    let version = String::from_utf8_lossy(&output.stdout);
    ensure!(
        output.status.success() && !version.contains("-desktop"),
        "docker buildx resolves to a Docker Desktop plugin ({}); set DOCKER_CONFIG to a directory whose \
         cli-plugins links the engine's docker-buildx",
        version.trim()
    );

    Ok(())
}
