//! Engine access from one of the user's WSL distributions: its root file system mounted where the engine can bind
//! its paths, the published Docker CLI linked in, and `/var/run/docker.sock` relayed to the shared engine socket.

use std::fs::{self, Permissions};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt, chown, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail, ensure};
use contr_core::guest;
use contr_engine_proxy::Rewrites;
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::unistd::Group;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};

use crate::mounts::is_mount_point;

const DOCKER_SOCKET: &str = "/var/run/docker.sock";
const DOCKER_GROUP: &str = "docker";
const SOCKET_MODE: u32 = 0o660;
const DISTROS_MODE: u32 = 0o700;

/// A link to a published Docker CLI file, placed only where the distribution has no package of its own.
struct CliLink {
    link: &'static str,
    published: &'static str,
    packaged: &'static [&'static str],
}

const CLI_LINKS: [CliLink; 3] = [
    CliLink {
        link: "/usr/local/bin/docker",
        published: "bin/docker",
        packaged: &["/usr/bin/docker", "/bin/docker"],
    },
    CliLink {
        link: "/usr/local/lib/docker/cli-plugins/docker-compose",
        published: "cli-plugins/docker-compose",
        packaged: &[
            "/usr/local/libexec/docker/cli-plugins/docker-compose",
            "/usr/lib/docker/cli-plugins/docker-compose",
            "/usr/libexec/docker/cli-plugins/docker-compose",
        ],
    },
    CliLink {
        link: "/usr/local/lib/docker/cli-plugins/docker-buildx",
        published: "cli-plugins/docker-buildx",
        packaged: &[
            "/usr/local/libexec/docker/cli-plugins/docker-buildx",
            "/usr/lib/docker/cli-plugins/docker-buildx",
            "/usr/libexec/docker/cli-plugins/docker-buildx",
        ],
    },
];

/// Serves the engine to `distribution`, the distribution this process runs in, through a socket owned by `owner`
/// until standard input closes or SIGTERM arrives, then removes everything it added.
pub(crate) async fn integrate(distribution: &str, owner: u32) -> anyhow::Result<()> {
    ensure!(
        is_plain_name(distribution),
        "{distribution:?} is not a distribution name"
    );
    let root = RootShare::mount(distribution)?;
    let _links = CliLinks::place();
    let socket = EngineSocket::bind(owner)?;
    let rewrites = Arc::new(Rewrites {
        drives_root: guest::WINDOWS_DRIVES.to_owned(),
        distro_root: Some(root.path.to_string_lossy().into_owned()),
        host_alias: true,
    });

    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(format!("{}\n", guest::INTEGRATION_READY).as_bytes())
        .await?;
    stdout.flush().await?;
    tracing::info!(%distribution, "serving the engine");

    let mut terminate = signal(SignalKind::terminate())?;
    let host_closed = async { tokio::io::copy(&mut tokio::io::stdin(), &mut tokio::io::sink()).await };
    tokio::pin!(host_closed);
    loop {
        tokio::select! {
            accepted = socket.listener.accept() => match accepted {
                Ok((client, _)) => {
                    tokio::spawn(relay(client, Arc::clone(&rewrites)));
                }
                Err(error) => tracing::warn!(%error, "cannot accept a Docker client"),
            },
            _ = &mut host_closed => break,
            _ = terminate.recv() => break,
        }
    }
    tracing::info!(%distribution, "no longer serving the engine");

    Ok(())
}

async fn relay(client: UnixStream, rewrites: Arc<Rewrites>) {
    let connect = || UnixStream::connect(guest::SHARED_ENGINE_SOCKET);
    if let Err(error) = contr_engine_proxy::serve(client, connect, rewrites).await {
        tracing::debug!(%error, "Docker client ended");
    }
}

/// WSL distribution names are single path components.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

/// The distribution's root file system, without its submounts, at its name in [`guest::DISTROS_DIR`]; unmounted on
/// drop.
struct RootShare {
    path: PathBuf,
}

impl RootShare {
    fn mount(distribution: &str) -> anyhow::Result<Self> {
        let dir = Path::new(guest::DISTROS_DIR);
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        fs::set_permissions(dir, Permissions::from_mode(DISTROS_MODE))?;
        let path = dir.join(distribution);
        fs::create_dir_all(&path).with_context(|| format!("cannot create {}", path.display()))?;

        if is_mount_point(&path)? {
            umount2(&path, MntFlags::MNT_DETACH).with_context(|| format!("cannot unmount {}", path.display()))?;
        }
        mount(Some("/"), &path, None::<&str>, MsFlags::MS_BIND, None::<&str>)
            .with_context(|| format!("cannot mount the root file system at {}", path.display()))?;

        Ok(Self { path })
    }
}

impl Drop for RootShare {
    fn drop(&mut self) {
        if let Err(error) = umount2(&self.path, MntFlags::MNT_DETACH) {
            tracing::warn!(%error, path = %self.path.display(), "cannot unmount the root file system");
            return;
        }
        let _ = fs::remove_dir(&self.path);
    }
}

/// The links this process placed; removed on drop.
struct CliLinks(Vec<&'static Path>);

impl CliLinks {
    fn place() -> Self {
        let placed = CLI_LINKS
            .iter()
            .filter_map(|cli| match cli.place() {
                Ok(placed) => placed.then(|| Path::new(cli.link)),
                Err(error) => {
                    tracing::warn!(%error, link = cli.link, "cannot link the Docker CLI");
                    None
                }
            })
            .collect();

        Self(placed)
    }
}

impl Drop for CliLinks {
    fn drop(&mut self) {
        for link in self.0.iter().filter(|link| is_published_link(link)) {
            let _ = fs::remove_file(link);
        }
    }
}

impl CliLink {
    fn place(&self) -> io::Result<bool> {
        let packaged: Vec<&Path> = self.packaged.iter().map(Path::new).collect();

        place_link(
            Path::new(self.link),
            &Path::new(guest::INTEGRATION_DIR).join(self.published),
            &packaged,
        )
    }
}

/// Links `link` to `target` unless the distribution has a package at one of `packaged` or its own file at `link`;
/// returns whether the link is in place.
fn place_link(link: &Path, target: &Path, packaged: &[&Path]) -> io::Result<bool> {
    if is_published_link(link) {
        fs::remove_file(link)?;
    }
    if packaged.iter().any(|path| path.exists()) || link.symlink_metadata().is_ok() {
        return Ok(false);
    }

    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    symlink(target, link)?;
    Ok(true)
}

fn is_published_link(link: &Path) -> bool {
    fs::read_link(link).is_ok_and(|target| target.starts_with(guest::INTEGRATION_DIR))
}

/// The distribution's Docker socket, owned by the default user and the `docker` group when the distribution has
/// one; removed on drop unless something else replaced it.
struct EngineSocket {
    listener: UnixListener,
    inode: (u64, u64),
}

impl EngineSocket {
    fn bind(owner: u32) -> anyhow::Result<Self> {
        let path = Path::new(DOCKER_SOCKET);
        if path.symlink_metadata().is_ok() {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                bail!("another engine serves {DOCKER_SOCKET} in this distribution");
            }
            fs::remove_file(path).with_context(|| format!("cannot remove the stale {DOCKER_SOCKET}"))?;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(path).with_context(|| format!("cannot listen on {DOCKER_SOCKET}"))?;
        let group = Group::from_name(DOCKER_GROUP)?.map_or(0, |group| group.gid.as_raw());
        chown(path, Some(owner), Some(group)).with_context(|| format!("cannot hand {DOCKER_SOCKET} to {owner}"))?;
        fs::set_permissions(path, Permissions::from_mode(SOCKET_MODE))?;
        let metadata = path.metadata()?;

        Ok(Self {
            listener,
            inode: (metadata.dev(), metadata.ino()),
        })
    }
}

impl Drop for EngineSocket {
    fn drop(&mut self) {
        let path = Path::new(DOCKER_SOCKET);
        if path
            .symlink_metadata()
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.inode)
        {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_single_component_names() {
        assert!(is_plain_name("Ubuntu-24.04"));
        assert!(!is_plain_name(""));
        assert!(!is_plain_name(".."));
        assert!(!is_plain_name("a/b"));
    }

    #[test]
    fn links_only_where_the_distribution_has_nothing_of_its_own() {
        let root = tempfile::tempdir().unwrap();
        let target = Path::new(guest::INTEGRATION_DIR).join("bin/docker");
        let link = root.path().join("local/bin/docker");
        let package = root.path().join("usr/bin/docker");
        let own = root.path().join("own");
        fs::write(&own, "").unwrap();

        assert!(place_link(&link, &target, &[]).unwrap());
        assert!(is_published_link(&link));
        assert!(place_link(&link, &target, &[]).unwrap());
        assert!(!place_link(&own, &target, &[]).unwrap());
        assert!(!is_published_link(&own));

        fs::create_dir_all(package.parent().unwrap()).unwrap();
        fs::write(&package, "").unwrap();
        assert!(!place_link(&link, &target, &[&package]).unwrap());
        assert!(link.symlink_metadata().is_err());
    }
}
