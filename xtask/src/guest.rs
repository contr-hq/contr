use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use contr_core::guest;

/// Exports the runtime and data guests as WSL-importable tarballs and writes their manifest.
pub(crate) fn build(root: &Path, arch: &str) -> anyhow::Result<()> {
    ensure_engine_buildx()?;
    let out = root.join("target/guest");
    fs::create_dir_all(&out).with_context(|| format!("cannot create {}", out.display()))?;
    let release = release(root)?;

    let runtime = format!("contr-runtime-{arch}.tar");
    let data = format!("contr-data-{arch}.tar");
    for (target, file) in [("runtime", &runtime), ("data", &data)] {
        let status = Command::new("docker")
            .args(["buildx", "build", "--file"])
            .arg(root.join("guest/Dockerfile"))
            .args(["--platform", &format!("linux/{arch}"), "--target", target])
            .args(["--build-arg", &format!("CONTR_RELEASE={release}")])
            .arg("--output")
            .arg(format!("type=tar,dest={}", out.join(file).display()))
            .arg(root)
            .status()
            .context("cannot run docker buildx")?;
        ensure!(status.success(), "building the {target} guest failed with {status}");
    }

    let manifest = serde_json::json!({ "release": release, "runtime": runtime, "data": data });
    fs::write(
        out.join(guest::MANIFEST_FILE),
        serde_json::to_string_pretty(&manifest)? + "\n",
    )?;
    println!("{release}: {}", out.display());
    Ok(())
}

/// Refuses Docker Desktop's buildx plugin, which shadows the engine's own plugin where both are installed;
/// see CLAUDE.md.
fn ensure_engine_buildx() -> anyhow::Result<()> {
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

/// Workspace version plus the commit. A build from uncommitted changes also carries its build time, so `contrd`
/// replaces the runtime after every such build.
fn release(root: &Path) -> anyhow::Result<String> {
    let commit = git(root, &["rev-parse", "--short=12", "HEAD"])?;
    let dirty = !git(root, &["status", "--porcelain"])?.is_empty();
    let suffix = if dirty {
        format!(".dirty.{}", SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
    } else {
        String::new()
    };

    Ok(format!("{}+{commit}{suffix}", env!("CARGO_PKG_VERSION")))
}

fn git(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("cannot run git")?;
    ensure!(output.status.success(), "git {} failed", args.join(" "));

    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
