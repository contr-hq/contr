//! What the runtime guest provides to the user's integrated WSL distributions.

use std::fs;
use std::path::Path;

use anyhow::Context;
use contr_core::guest;
use nix::mount::{MntFlags, umount2};

use crate::mounts::mount_points_under;

/// Files integrated distributions run: their path in the runtime guest and in [`guest::INTEGRATION_DIR`].
const PUBLISHED: [(&str, &str); 4] = [
    (guest::AGENT_PATH, "bin/contr-agent"),
    ("/usr/local/bin/docker", "bin/docker"),
    (
        "/usr/local/libexec/docker/cli-plugins/docker-compose",
        "cli-plugins/docker-compose",
    ),
    (
        "/usr/local/libexec/docker/cli-plugins/docker-buildx",
        "cli-plugins/docker-buildx",
    ),
];
const RELEASE: &str = "release";

/// Creates the directory of the shared engine socket, releases what earlier integrations left mounted, and
/// publishes the files integrated distributions run unless this release already did.
pub(crate) fn prepare() -> anyhow::Result<()> {
    let socket_dir = Path::new(guest::SHARED_ENGINE_SOCKET)
        .parent()
        .context("the shared engine socket has no directory")?;
    fs::create_dir_all(socket_dir).with_context(|| format!("cannot create {}", socket_dir.display()))?;
    release_distributions(None)?;

    publish()
}

/// Unmounts the root file system of `distribution`, or of every distribution, that an integration left mounted
/// when it ended without cleaning up. The unmount reaches every distribution, which all share `/mnt/wsl`.
pub(crate) fn release_distributions(distribution: Option<&str>) -> anyhow::Result<()> {
    let dir = Path::new(guest::DISTROS_DIR);
    let mounted = mount_points_under(dir).context("cannot list mounts")?;
    let released = mounted
        .iter()
        .filter(|point| distribution.is_none_or(|name| *point == &dir.join(name)));
    for point in released {
        umount2(point, MntFlags::MNT_DETACH).with_context(|| format!("cannot unmount {}", point.display()))?;
        tracing::info!(point = %point.display(), "released a distribution mount");
    }

    Ok(())
}

fn publish() -> anyhow::Result<()> {
    let dir = Path::new(guest::INTEGRATION_DIR);
    let release =
        fs::read_to_string(guest::RELEASE_FILE).with_context(|| format!("cannot read {}", guest::RELEASE_FILE))?;
    if fs::read_to_string(dir.join(RELEASE)).is_ok_and(|published| published == release) {
        return Ok(());
    }

    for (source, published) in PUBLISHED {
        let target = dir.join(published);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let staging = target.with_extension("new");
        fs::copy(source, &staging).with_context(|| format!("cannot copy {source}"))?;
        fs::rename(&staging, &target).with_context(|| format!("cannot replace {}", target.display()))?;
    }
    fs::write(dir.join(RELEASE), &release).context("cannot record the published release")?;
    tracing::info!(
        release = release.trim(),
        "published the files integrated distributions run"
    );

    Ok(())
}
