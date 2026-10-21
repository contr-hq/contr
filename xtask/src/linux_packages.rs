//! Debian and RPM packages of the Linux archive, which nfpm builds in packaging/linux/package.Dockerfile.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, ensure};
use contr_core::product;

use crate::{dist, docker};

/// Builds the packages of `version` from the archive in `out` and writes them into `out`.
pub(crate) fn build(root: &Path, version: &str, out: &Path) -> anyhow::Result<()> {
    docker::ensure_engine_buildx()?;
    let archive = out.join(dist::LINUX);
    ensure!(
        archive.join(product::DAEMON_BINARY).is_file(),
        "{} holds no Linux archive; run cargo xtask dist-linux first",
        out.display()
    );

    let status = Command::new("docker")
        .args(["buildx", "build", "--file"])
        .arg(root.join("packaging/linux/package.Dockerfile"))
        .arg("--build-context")
        .arg(format!("archive={}", archive.display()))
        .arg("--build-arg")
        .arg(format!("VERSION={version}"))
        .arg("--output")
        .arg(format!("type=local,dest={}", out.display()))
        .arg(root.join("packaging/linux"))
        .status()
        .context("cannot run docker buildx")?;
    ensure!(status.success(), "building the Linux packages failed with {status}");

    println!("{}", out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use contr_core::product;

    const NFPM: &str = include_str!("../../packaging/linux/nfpm.yaml");

    fn installs(src: &str, dst: &str) -> bool {
        NFPM.contains(&format!("- src: {src}\n    dst: {dst}\n"))
    }

    #[test]
    fn packages_install_files_where_the_code_looks_for_them() {
        let unit = include_str!("../../packaging/linux/systemd/contrd.service");
        let daemon = format!("/usr/bin/{}", product::DAEMON_BINARY);

        assert!(NFPM.contains(&format!("\nname: {}\n", product::DAEMON_PACKAGE)));
        assert!(installs(&format!("archive/{}", product::DAEMON_BINARY), &daemon));
        assert!(unit.contains(&format!("\nExecStart={daemon}\n")));
        assert!(installs(
            &format!("archive/{}", product::CLI_BINARY),
            &format!("/usr/bin/{}", product::CLI_BINARY)
        ));
        assert!(installs(
            &format!("archive/{}", product::HELPER_BINARY),
            product::INSTALLED_HELPER
        ));
        assert!(installs(
            &format!("linux/systemd/{}", product::DAEMON_UNIT),
            &format!("/usr/lib/systemd/user/{}", product::DAEMON_UNIT)
        ));
    }
}
