//! License notices that ship with Contr's release archives: `LICENSE`, `THIRD-PARTY-NOTICES.md` and `licenses/`,
//! generated from what the archive holds so their versions cannot drift from it.

use std::collections::BTreeSet;
use std::fmt::Write;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, ensure};
use serde_json::Value;

/// Where `cargo about generate` writes the Rust crate notices before an archive is assembled.
pub(crate) const RUST_CRATES: &str = "target/notices/rust-crates.txt";

/// Bundled CLI tools by their key in `cli.json`: name, license and source repository.
const CLI_TOOLS: [(&str, &str, &str, &str); 5] = [
    ("docker", "Docker CLI", "Apache-2.0", "https://github.com/docker/cli"),
    (
        "compose",
        "Docker Compose",
        "Apache-2.0",
        "https://github.com/docker/compose",
    ),
    (
        "buildx",
        "Docker Buildx",
        "Apache-2.0",
        "https://github.com/docker/buildx",
    ),
    (
        "credential-wincred",
        "docker-credential-wincred",
        "MIT",
        "https://github.com/docker/docker-credential-helpers",
    ),
    (
        "kubectl",
        "kubectl",
        "Apache-2.0",
        "https://github.com/kubernetes/kubernetes",
    ),
];

/// What an archive carries besides Contr's own code.
pub(crate) struct Contents<'a> {
    /// `cli.json` of the bundled Docker CLI tools.
    pub(crate) cli_tools: Option<&'a Value>,
    /// The WSL guest images.
    pub(crate) guest_images: &'a [&'a Path],
}

/// Writes the notices into `dir`.
pub(crate) fn write(root: &Path, dir: &Path, contents: &Contents<'_>) -> anyhow::Result<()> {
    let licenses = dir.join("licenses");
    fs::create_dir_all(&licenses).with_context(|| format!("cannot create {}", licenses.display()))?;
    fs::copy(root.join("LICENSE"), dir.join("LICENSE")).context("cannot copy LICENSE")?;
    let rust_crates = root.join(RUST_CRATES);
    ensure!(
        rust_crates.is_file(),
        "{RUST_CRATES} is missing; run `cargo about generate --config packaging/licenses/about.toml \
         --output-file {RUST_CRATES} packaging/licenses/about.hbs`"
    );
    fs::copy(&rust_crates, licenses.join("rust-crates.txt")).context("cannot copy the Rust crate notices")?;

    let mut notices = String::from(
        "# Third-party software in Contr\n\nContr is licensed under the Apache License 2.0 (`LICENSE`). This archive \
         also carries the following software, each under its own license.\n",
    );
    if let Some(cli_tools) = contents.cli_tools {
        cli_section(&mut notices, cli_tools)?;
    }
    if !contents.guest_images.is_empty() {
        let packages = guest_packages(contents.guest_images)?;
        fs::write(licenses.join("guest-packages.txt"), package_table(&packages))?;
        guest_section(&mut notices, &dockerfile_versions(root)?, &packages)?;
    }
    notices.push_str(
        "\n## Rust crates\n\nContr's executables are built from the Rust crates listed with their licenses and \
         license texts in `licenses/rust-crates.txt`.\n",
    );

    fs::write(dir.join("THIRD-PARTY-NOTICES.md"), notices)?;
    Ok(())
}

fn cli_section(notices: &mut String, cli_tools: &Value) -> anyhow::Result<()> {
    notices.push_str("\n## CLI tools (`cli/`)\n\n| Component | Version | License | Source |\n|---|---|---|---|\n");
    for (key, name, license, repository) in CLI_TOOLS {
        let version = cli_tools[key]
            .as_str()
            .with_context(|| format!("cli.json has no {key}"))?;
        writeln!(
            notices,
            "| {name} | {version} | {license} | {repository}/tree/v{version} |"
        )?;
    }

    Ok(())
}

fn guest_section(notices: &mut String, versions: &Versions, packages: &[Package]) -> anyhow::Result<()> {
    let alpine = packages
        .iter()
        .find(|package| package.name == "alpine-release")
        .map_or("unknown", |package| {
            package.version.split('-').next().unwrap_or_default()
        });
    write!(
        notices,
        "\n## WSL guest images (`guest/`)\n\n\
         | Component | Version | License | Source |\n|---|---|---|---|\n\
         | Moby: dockerd, docker-proxy, containerd, containerd-shim-runc-v2, ctr, runc | {moby} | Apache-2.0 | \
         https://github.com/moby/moby/tree/docker-v{moby} |\n\
         | docker-init (tini) | as bundled with Moby {moby} | MIT | https://github.com/krallin/tini |\n\
         | Docker Compose for Linux | {compose} | Apache-2.0 | https://github.com/docker/compose/tree/v{compose} |\n\
         | Docker Buildx for Linux | {buildx} | Apache-2.0 | https://github.com/docker/buildx/tree/v{buildx} |\n\
         | Alpine Linux packages | {alpine} | see `licenses/guest-packages.txt` | \
         https://gitlab.alpinelinux.org/alpine/aports |\n\n\
         The release that publishes this archive also publishes `contr-guest-sources-<version>.tar.gz`: for each \
         Alpine package listed in `licenses/guest-packages.txt`, the aports directory at the commit the package was \
         built from and every source file it names, from which Alpine's abuild builds the package.\n",
        moby = versions.moby,
        compose = versions.compose,
        buildx = versions.buildx,
    )?;

    Ok(())
}

struct Versions {
    moby: String,
    compose: String,
    buildx: String,
}

/// Engine, Compose and Buildx versions the guest Dockerfile pins.
fn dockerfile_versions(root: &Path) -> anyhow::Result<Versions> {
    let dockerfile = fs::read_to_string(root.join("guest/Dockerfile")).context("cannot read guest/Dockerfile")?;
    let pinned = |name: &str| {
        dockerfile
            .lines()
            .find_map(|line| line.trim().strip_prefix(&format!("ARG {name}=")))
            .map(str::to_owned)
            .with_context(|| format!("guest/Dockerfile pins no {name}"))
    };

    Ok(Versions {
        moby: pinned("MOBY_VERSION")?,
        compose: pinned("COMPOSE_VERSION")?,
        buildx: pinned("BUILDX_VERSION")?,
    })
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Package {
    name: String,
    version: String,
    license: String,
}

/// Alpine packages installed in any of `images`, read from each image's package database.
fn guest_packages(images: &[&Path]) -> anyhow::Result<Vec<Package>> {
    let mut packages = BTreeSet::new();
    for image in images {
        let output = Command::new("tar")
            .arg("-xOf")
            .arg(image)
            .arg("lib/apk/db/installed")
            .output()
            .context("cannot run tar")?;
        ensure!(
            output.status.success(),
            "cannot read the package database of {}",
            image.display()
        );
        packages.extend(parse_installed(&String::from_utf8_lossy(&output.stdout)));
    }

    Ok(packages.into_iter().collect())
}

/// Packages of an apk `installed` database, whose records hold `P:` name, `V:` version and `L:` license lines.
fn parse_installed(database: &str) -> Vec<Package> {
    database
        .split("\n\n")
        .filter_map(|record| {
            let field = |key: &str| {
                record
                    .lines()
                    .find_map(|line| line.strip_prefix(key))
                    .map(str::to_owned)
            };
            Some(Package {
                name: field("P:")?,
                version: field("V:")?,
                license: field("L:").unwrap_or_default(),
            })
        })
        .collect()
}

fn package_table(packages: &[Package]) -> String {
    let width = packages.iter().map(|package| package.name.len()).max().unwrap_or(0);
    let version_width = packages.iter().map(|package| package.version.len()).max().unwrap_or(0);

    packages.iter().fold(
        String::from("Alpine Linux packages in Contr's WSL guest images: name, version, license.\n\n"),
        |mut table, package| {
            let _ = writeln!(
                table,
                "{:width$}  {:version_width$}  {}",
                package.name, package.version, package.license
            );
            table
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The quoted names between `start` and the next `]` in `text`.
    fn quoted_list(text: &str, start: &str) -> Vec<String> {
        let list = text.split_once(start).unwrap().1.split_once(']').unwrap().0;
        let mut names: Vec<String> = list.split('"').skip(1).step_by(2).map(str::to_owned).collect();
        names.sort();
        names
    }

    #[test]
    fn notices_accept_the_licences_cargo_deny_allows() {
        let deny = include_str!("../../deny.toml");
        let about = include_str!("../../packaging/licenses/about.toml");

        assert_eq!(quoted_list(about, "accepted = ["), quoted_list(deny, "\nallow = ["));
    }

    #[test]
    fn reads_package_records() {
        let database = "C:Q1abc=\nP:busybox\nV:1.37.0-r31\nA:x86_64\nL:GPL-2.0-only\n\nP:musl\nV:1.2.6-r2\nL:MIT\n\n";

        assert_eq!(
            parse_installed(database),
            [
                Package {
                    name: "busybox".to_owned(),
                    version: "1.37.0-r31".to_owned(),
                    license: "GPL-2.0-only".to_owned(),
                },
                Package {
                    name: "musl".to_owned(),
                    version: "1.2.6-r2".to_owned(),
                    license: "MIT".to_owned(),
                },
            ]
        );
    }
}
