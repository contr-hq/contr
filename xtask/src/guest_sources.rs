//! The exact source of every Alpine package in the exported guest images, fetched by guest/sources/fetch.sh as root
//! in the Alpine image the guests are built on, into target/guest-sources/contr-guest-sources, which the script hands
//! back to the workspace's owner.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, ensure};
use contr_core::guest;
use serde_json::Value;

/// Reads the package databases of the images in target/guest and fetches their packages' sources.
pub(crate) fn fetch(root: &Path) -> anyhow::Result<()> {
    let guest_dir = root.join("target/guest");
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(guest_dir.join(guest::MANIFEST_FILE))
            .context("cannot read the guest manifest; run `cargo xtask guest` first")?,
    )?;
    let work = root.join("target/guest-sources");
    if work.exists() {
        fs::remove_dir_all(&work).with_context(|| format!("cannot clear {}", work.display()))?;
    }
    fs::create_dir_all(work.join("db"))?;

    let mut databases = Vec::new();
    for key in ["runtime", "data"] {
        let image = manifest[key]
            .as_str()
            .with_context(|| format!("the guest manifest has no {key}"))?;
        let output = Command::new("tar")
            .arg("-xOf")
            .arg(guest_dir.join(image))
            .arg("lib/apk/db/installed")
            .output()
            .context("cannot run tar")?;
        ensure!(output.status.success(), "cannot read the package database of {image}");
        fs::write(work.join("db").join(key), &output.stdout)?;
        databases.push(format!("/work/db/{key}"));
    }
    let branch = alpine_branch(&fs::read_to_string(work.join("db/runtime"))?)?;

    let mut command = Command::new("docker");
    command.args(["run", "--rm"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let owner = fs::metadata(root)?;
        command.args(["--env", &format!("OWNER={}:{}", owner.uid(), owner.gid())]);
    }
    let status = command
        .arg("--volume")
        .arg(format!("{}:/work", work.display()))
        .arg("--volume")
        .arg(format!("{}:/script:ro", root.join("guest/sources").display()))
        .arg(alpine_image(root)?)
        .args(["sh", "/script/fetch.sh", "/work/contr-guest-sources", &branch])
        .args(&databases)
        .status()
        .context("cannot run docker")?;
    ensure!(status.success(), "fetching the guest sources failed with {status}");

    println!("{}", work.join("contr-guest-sources").display());
    Ok(())
}

/// The Alpine image guest/Dockerfile builds on.
fn alpine_image(root: &Path) -> anyhow::Result<String> {
    let dockerfile = fs::read_to_string(root.join("guest/Dockerfile")).context("cannot read guest/Dockerfile")?;
    dockerfile
        .lines()
        .find_map(|line| line.strip_prefix("ARG ALPINE="))
        .map(str::to_owned)
        .context("guest/Dockerfile names no ALPINE image")
}

/// Alpine's branch, such as `v3.24`, from the `alpine-release` package of a package database.
fn alpine_branch(database: &str) -> anyhow::Result<String> {
    let version = database
        .split("\n\n")
        .find(|record| record.lines().any(|line| line == "P:alpine-release"))
        .and_then(|record| record.lines().find_map(|line| line.strip_prefix("V:")))
        .context("the guest has no alpine-release package")?;
    let mut parts = version.split(['.', '-']);

    Ok(format!(
        "v{}.{}",
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_branch_from_alpine_release() {
        let database = "P:musl\nV:1.2.6-r2\n\nP:alpine-release\nV:3.24.2-r0\nL:MIT\n\n";

        assert_eq!(alpine_branch(database).unwrap(), "v3.24");
    }
}
