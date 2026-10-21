//! Privileged Linux helper run through pkexec for an allow-listed set of operations. It acts only for the user who ran
//! pkexec, whom `PKEXEC_UID` names, and runs only the commands below. Each step goes to standard output as one line;
//! the commands' own output goes to standard error.

use std::process::ExitCode;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use contr_core::product;

#[derive(Parser)]
#[command(name = product::HELPER_BINARY, version, about)]
struct Args {
    #[command(subcommand)]
    operation: Operation,
}

#[derive(Subcommand)]
enum Operation {
    /// Install the distribution's container engine packages, start the engine service and add the user to the docker
    /// group.
    InstallEngine,
    /// Add the user to the docker group.
    JoinDockerGroup,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args.operation) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(operation: &Operation) -> anyhow::Result<()> {
    let user = invoking_user()?;
    if let Operation::InstallEngine = operation {
        let os_release = std::fs::read_to_string("/etc/os-release").context("cannot read /etc/os-release")?;
        let distribution = Distribution::detect(&os_release)?;
        step(&format!("Installing {}", distribution.packages().join(" ")));
        for command in distribution.install_commands() {
            execute(&command)?;
        }
        step("Starting docker.service");
        execute(&["systemctl", "enable", "--now", "docker.service"])?;
    }
    step("Adding you to the docker group");
    execute(&["groupadd", "--force", "docker"])?;
    execute(&["usermod", "--append", "--groups", "docker", &user])
}

/// Name of the user who ran pkexec.
#[cfg(unix)]
fn invoking_user() -> anyhow::Result<String> {
    let uid: u32 = std::env::var("PKEXEC_UID")
        .context("run me through pkexec")?
        .parse()
        .context("PKEXEC_UID is not a user ID")?;
    let user = nix::unistd::User::from_uid(uid.into())
        .context("cannot look up the user")?
        .with_context(|| format!("no user has ID {uid}"))?;

    Ok(user.name)
}

fn step(message: &str) {
    println!("{message}");
}

#[cfg(not(unix))]
fn invoking_user() -> anyhow::Result<String> {
    bail!("{} runs only on Linux", product::HELPER_BINARY)
}

/// Runs `command`, sending its output to standard error.
#[cfg(unix)]
fn execute(command: &[&str]) -> anyhow::Result<()> {
    use std::os::fd::AsFd;
    use std::process::{Command, Stdio};

    let (program, args) = command.split_first().context("an empty command")?;
    let stderr = std::io::stderr().as_fd().try_clone_to_owned()?;
    let status = Command::new(program)
        .args(args)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stderr))
        .status()
        .with_context(|| format!("cannot run {program}"))?;
    anyhow::ensure!(status.success(), "{} failed with {status}", command.join(" "));

    Ok(())
}

#[cfg(not(unix))]
fn execute(command: &[&str]) -> anyhow::Result<()> {
    bail!("cannot run {} here", command.join(" "))
}

/// Distribution families whose own repositories carry a Docker Engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Distribution {
    Ubuntu,
    Debian,
    Fedora,
}

impl Distribution {
    /// The family `os_release`, the content of `/etc/os-release`, belongs to.
    fn detect(os_release: &str) -> anyhow::Result<Self> {
        let field = |key: &str| {
            os_release
                .lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
                .map(|value| value.trim().trim_matches('"').to_owned())
                .unwrap_or_default()
        };
        let id = field("ID");
        let like = field("ID_LIKE");
        let family = |name: &str| id == name || like.split_whitespace().any(|like| like == name);

        if family("ubuntu") {
            Ok(Self::Ubuntu)
        } else if family("debian") {
            Ok(Self::Debian)
        } else if family("fedora") {
            Ok(Self::Fedora)
        } else {
            let name = Some(field("PRETTY_NAME")).filter(|name| !name.is_empty()).unwrap_or(id);
            bail!("Contr cannot install an engine on {name}; install Docker Engine with its package manager")
        }
    }

    fn packages(self) -> &'static [&'static str] {
        match self {
            Self::Ubuntu => &["docker.io", "docker-compose-v2", "docker-buildx"],
            Self::Debian => &["docker.io", "docker-compose", "docker-buildx"],
            Self::Fedora => &["moby-engine", "docker-compose"],
        }
    }

    fn install_commands(self) -> Vec<Vec<&'static str>> {
        let install = |manager: &[&'static str]| manager.iter().chain(self.packages()).copied().collect();
        match self {
            Self::Ubuntu | Self::Debian => vec![vec!["apt-get", "update"], install(&["apt-get", "install", "--yes"])],
            Self::Fedora => vec![install(&["dnf", "install", "--assumeyes"])],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_distribution_families() {
        let detect = |text: &str| Distribution::detect(text).map_err(|error| error.to_string());

        assert_eq!(detect("ID=ubuntu\nID_LIKE=debian\n"), Ok(Distribution::Ubuntu));
        assert_eq!(
            detect("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n"),
            Ok(Distribution::Ubuntu)
        );
        assert_eq!(detect("ID=debian\n"), Ok(Distribution::Debian));
        assert_eq!(detect("ID=fedora\n"), Ok(Distribution::Fedora));
        assert_eq!(
            detect("ID=arch\nPRETTY_NAME=\"Arch Linux\"\n"),
            Err(
                "Contr cannot install an engine on Arch Linux; install Docker Engine with its package manager"
                    .to_owned()
            )
        );
    }

    #[test]
    fn installs_with_the_familys_package_manager() {
        assert_eq!(
            Distribution::Ubuntu.install_commands(),
            [
                vec!["apt-get", "update"],
                vec![
                    "apt-get",
                    "install",
                    "--yes",
                    "docker.io",
                    "docker-compose-v2",
                    "docker-buildx"
                ],
            ]
        );
        assert_eq!(
            Distribution::Fedora.install_commands(),
            [vec!["dnf", "install", "--assumeyes", "moby-engine", "docker-compose"]]
        );
    }

    #[test]
    fn refuses_to_run_outside_pkexec() {
        if std::env::var_os("PKEXEC_UID").is_none() {
            assert!(invoking_user().is_err());
        }
    }
}
