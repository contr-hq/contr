//! Release archives. The Windows core payload, which the client's installer carries, holds the daemon and the CLI
//! built for Windows, the WSL guest images and the Docker CLI tools, laid out the way `contrd` finds them next to
//! itself, with a manifest that pins every file by SHA-256. The Linux archive holds the daemon and the CLI. Both carry
//! their license notices.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use contr_core::{guest, product};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::notices::{self, Contents};

/// Directory name of the payload inside `out`.
const PAYLOAD: &str = "contr-core-windows-x64";
/// Directory name of the Linux archive inside `out`.
const LINUX: &str = "contr-linux-x64";
/// Manifest at the payload's root.
const MANIFEST: &str = "core.json";
/// Manifest of the Docker CLI tools, which packaging/windows/cli.Dockerfile writes.
const CLI_MANIFEST: &str = "cli.json";

/// Assembles the payload from the Windows executables in `binaries`, target/guest and target/windows-cli.
pub(crate) fn windows(root: &Path, binaries: &Path, out: &Path) -> anyhow::Result<()> {
    let guest_dir = root.join("target/guest");
    let cli_dir = root.join("target/windows-cli");
    let payload = fresh_dir(&out.join(PAYLOAD))?;

    for binary in [product::DAEMON_BINARY, product::CLI_BINARY] {
        let file = format!("{binary}.exe");
        copy(&binaries.join(&file), &payload.join(&file))?;
    }

    let guest_manifest: Value = read_json(&guest_dir.join(guest::MANIFEST_FILE))?;
    let release = text(&guest_manifest, "release")?;
    let images = [text(&guest_manifest, "runtime")?, text(&guest_manifest, "data")?];
    for file in [guest::MANIFEST_FILE, images[0], images[1]] {
        copy(&guest_dir.join(file), &payload.join("guest").join(file))?;
    }
    copy_tree(&cli_dir, &payload.join("cli"))?;
    let cli_tools = read_json(&cli_dir.join(CLI_MANIFEST))?;
    let image_paths = images.map(|image| guest_dir.join(image));
    let image_paths: Vec<&Path> = image_paths.iter().map(PathBuf::as_path).collect();
    notices::write(
        root,
        &payload,
        &Contents {
            cli_tools: Some(&cli_tools),
            guest_images: &image_paths,
        },
    )?;

    let api_revision: u32 = fs::read_to_string(root.join("proto/API_REVISION"))
        .context("cannot read proto/API_REVISION")?
        .trim()
        .parse()
        .context("proto/API_REVISION is not a number")?;
    let manifest = json!({
        "version": release,
        "apiRevision": api_revision,
        "files": digests(&payload)?,
    });
    fs::write(payload.join(MANIFEST), serde_json::to_string_pretty(&manifest)? + "\n")?;

    println!("{release}: {}", payload.display());
    Ok(())
}

/// Assembles the Linux archive from the Linux executables in `binaries`.
pub(crate) fn linux(root: &Path, binaries: &Path, out: &Path) -> anyhow::Result<()> {
    let archive = fresh_dir(&out.join(LINUX))?;
    for binary in [product::DAEMON_BINARY, product::CLI_BINARY] {
        copy(&binaries.join(binary), &archive.join(binary))?;
    }
    notices::write(
        root,
        &archive,
        &Contents {
            cli_tools: None,
            guest_images: &[],
        },
    )?;

    println!("{}", archive.display());
    Ok(())
}

fn fresh_dir(dir: &Path) -> anyhow::Result<PathBuf> {
    if dir.exists() {
        fs::remove_dir_all(dir).with_context(|| format!("cannot clear {}", dir.display()))?;
    }
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;

    Ok(dir.to_path_buf())
}

fn read_json(path: &Path) -> anyhow::Result<Value> {
    let text = fs::read_to_string(path).with_context(|| format!("cannot read {}; build it first", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))
}

fn text<'a>(value: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("the guest manifest has no {key}"))
}

fn copy(from: &Path, to: &Path) -> anyhow::Result<()> {
    ensure!(from.is_file(), "{} is missing", from.display());
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    }
    fs::copy(from, to).with_context(|| format!("cannot copy {} to {}", from.display(), to.display()))?;

    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> anyhow::Result<()> {
    ensure!(from.is_dir(), "{} is missing; build it first", from.display());
    for file in files(from)? {
        copy(&from.join(&file), &to.join(&file))?;
    }

    Ok(())
}

/// Files under `dir`, relative to it, in a stable order.
fn files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        for entry in fs::read_dir(dir.join(&relative)).with_context(|| format!("cannot list {}", dir.display()))? {
            let entry = entry?;
            let path = relative.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                pending.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort();

    Ok(found)
}

/// SHA-256 of every file, keyed by its `/`-separated path.
fn digests(dir: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    files(dir)?
        .into_iter()
        .map(|file| {
            let bytes = fs::read(dir.join(&file)).with_context(|| format!("cannot read {}", file.display()))?;
            let key = file
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            Ok((key, hex(&Sha256::digest(bytes))))
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}
