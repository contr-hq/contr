use std::fs;
use std::path::Path;

use anyhow::Context;
use contr_core::guest;
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use tokio::io::AsyncWriteExt;

use crate::mounts::is_mount_point;

/// Bind-mounts the data guest's storage at [`guest::DATA_MOUNT`], reports readiness on standard output, and holds the
/// mount until standard input closes.
pub(crate) async fn anchor() -> anyhow::Result<()> {
    let source = Path::new(guest::DATA_SOURCE);
    let target = Path::new(guest::DATA_MOUNT);
    fs::create_dir_all(source).with_context(|| format!("cannot create {}", source.display()))?;
    fs::create_dir_all(target).with_context(|| format!("cannot create {}", target.display()))?;

    if !is_mount_point(target)? {
        mount(Some(source), target, None::<&str>, MsFlags::MS_BIND, None::<&str>)
            .with_context(|| format!("cannot bind {} to {}", source.display(), target.display()))?;
    }
    tracing::info!(target = %target.display(), "engine data mounted");

    let mut stdout = tokio::io::stdout();
    stdout.write_all(format!("{}\n", guest::DATA_READY).as_bytes()).await?;
    stdout.flush().await?;

    tokio::io::copy(&mut tokio::io::stdin(), &mut tokio::io::sink()).await?;

    if let Err(error) = umount2(target, MntFlags::MNT_DETACH) {
        tracing::warn!(%error, "cannot unmount engine data");
    }
    Ok(())
}
