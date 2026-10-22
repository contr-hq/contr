use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::{Output, Stdio};

use contr_core::guest;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `wsl.exe` asked for UTF-8; without `WSL_UTF8` it writes UTF-16.
fn wsl() -> Command {
    #[cfg(windows)]
    let program = contr_platform::elevation::system_program("wsl.exe").unwrap_or_else(|_| "wsl.exe".into());
    #[cfg(not(windows))]
    let program = "wsl.exe";
    let mut command = Command::new(program);
    command.env("WSL_UTF8", "1").stdin(Stdio::null());
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

async fn run<I, S>(args: I) -> io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    wsl().args(args).output().await
}

/// WSL version line, or `None` when no WSL answering `--version` is installed.
pub(crate) async fn version() -> Option<String> {
    let output = run(["--version"]).await.ok()?;

    output.status.success().then(|| {
        decode(&output.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned()
    })
}

/// What `wsl.exe` prints for `args`, standard output then standard error, or why it did not run.
pub(crate) async fn report(args: &[&str]) -> String {
    match run(args).await {
        Ok(output) => format!("{}{}", decode(&output.stdout), decode(&output.stderr)),
        Err(error) => format!("cannot run wsl.exe: {error}\n"),
    }
}

/// Names of the registered distributions.
pub(crate) async fn distributions() -> io::Result<Vec<String>> {
    let output = run(["--list", "--quiet"]).await?;

    // wsl.exe exits non-zero when no distribution is registered.
    Ok(if output.status.success() {
        names(&decode(&output.stdout))
    } else {
        Vec::new()
    })
}

/// A registered distribution as `wsl.exe --list --verbose` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Listed {
    pub(crate) name: String,
    pub(crate) version: u8,
    pub(crate) is_default: bool,
}

/// The registered distributions with their WSL version.
pub(crate) async fn list() -> io::Result<Vec<Listed>> {
    let output = run(["--list", "--verbose"]).await?;

    Ok(if output.status.success() {
        listed(&decode(&output.stdout))
    } else {
        Vec::new()
    })
}

/// User ID that `wsl.exe` runs commands in distribution `name` as when no user is named.
pub(crate) async fn default_uid(name: &str) -> io::Result<u32> {
    let output = run(["--distribution", name, "--exec", "id", "-u"]).await?;
    check(&output, &format!("find the default user of {name}"))?;

    decode(&output.stdout)
        .trim()
        .parse()
        .map_err(|_| io::Error::other(format!("{name} reported an invalid default user ID")))
}

/// Runs `args` as root in distribution `name` until it exits successfully.
pub(crate) async fn exec(name: &str, args: &[&str]) -> io::Result<()> {
    let output = wsl()
        .args(["--distribution", name, "--user", "root", "--exec"])
        .args(args)
        .output()
        .await?;

    check(&output, &format!("run {} in {name}", args.join(" ")))
}

/// Hands the engine in distribution `name` the configuration `config` for its next start, or with
/// [`Configure::Check`] only asks the engine whether it accepts it.
pub(crate) async fn configure(name: &str, config: &guest::EngineConfig, mode: Configure) -> Result<(), ConfigureError> {
    let mut command = wsl();
    command.args([
        "--distribution",
        name,
        "--user",
        "root",
        "--exec",
        guest::AGENT_PATH,
        "configure",
    ]);
    if mode == Configure::Check {
        command.arg("--check");
    }
    let mut agent = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = agent.stdin.take() {
        stdin
            .write_all(&serde_json::to_vec(config).map_err(io::Error::from)?)
            .await?;
    }
    let output = agent.wait_with_output().await?;
    if output.status.code() == Some(i32::from(guest::CONFIG_REJECTED)) {
        return Err(ConfigureError::Rejected(rejection(
            &decode(&output.stdout),
            &decode(&output.stderr),
        )));
    }

    Ok(check(&output, &format!("configure the engine in {name}"))?)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Configure {
    Apply,
    Check,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigureError {
    #[error("the engine rejects its configuration: {}", .0.reason)]
    Rejected(guest::ConfigRejection),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<ConfigureError> for io::Error {
    fn from(error: ConfigureError) -> Self {
        match error {
            ConfigureError::Io(error) => error,
            rejected @ ConfigureError::Rejected(_) => io::Error::other(rejected.to_string()),
        }
    }
}

/// The rejection the agent wrote to `stdout`, or else its error message in `stderr`, which may also hold warnings
/// `wsl.exe` printed first.
fn rejection(stdout: &str, stderr: &str) -> guest::ConfigRejection {
    if let Some(rejection) = stdout.lines().rev().find_map(|line| serde_json::from_str(line).ok()) {
        return rejection;
    }
    let message = stderr
        .find("Error: ")
        .map_or(stderr, |start| &stderr[start + "Error: ".len()..]);

    guest::ConfigRejection {
        reason: message.trim().to_owned(),
        unknown_keys: Vec::new(),
    }
}

/// Registers `image` as WSL 2 distribution `name`, with its virtual disk in `install_dir`.
pub(crate) async fn import(name: &str, install_dir: &Path, image: &Path) -> io::Result<()> {
    std::fs::create_dir_all(install_dir)?;
    let args = [
        OsStr::new("--import"),
        OsStr::new(name),
        install_dir.as_os_str(),
        image.as_os_str(),
        OsStr::new("--version"),
        OsStr::new("2"),
    ];

    check(&run(args).await?, &format!("import {name}"))
}

/// Unregisters distribution `name`, deleting its virtual disk.
pub(crate) async fn unregister(name: &str) -> io::Result<()> {
    check(&run(["--unregister", name]).await?, &format!("unregister {name}"))
}

/// Release recorded in distribution `name` when its image was built.
pub(crate) async fn release(name: &str) -> io::Result<String> {
    let output = run([
        "--distribution",
        name,
        "--user",
        "root",
        "--exec",
        "cat",
        guest::RELEASE_FILE,
    ])
    .await?;
    let release = decode(&output.stdout).trim().to_owned();
    check(&output, &format!("read the release of {name}"))?;

    Ok(release)
}

/// Starts `args` as root in distribution `name` with piped standard streams; the process ends when the child is
/// dropped.
pub(crate) fn spawn(name: &str, args: &[&str]) -> io::Result<Child> {
    wsl()
        .args(["--distribution", name, "--user", "root", "--exec"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

fn check(output: &Output, action: &str) -> io::Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let mut message = decode(&output.stdout);
    message.push_str(&decode(&output.stderr));

    Err(io::Error::other(format!(
        "wsl.exe could not {action} ({}): {}",
        output.status,
        message.trim()
    )))
}

/// Text written by `wsl.exe`, which ignores `WSL_UTF8` in some builds and writes UTF-16LE.
fn decode(bytes: &[u8]) -> String {
    let utf16 =
        bytes.len() >= 2 && bytes.len().is_multiple_of(2) && bytes.iter().skip(1).step_by(2).all(|byte| *byte == 0);
    if !utf16 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();

    String::from_utf16_lossy(&units)
}

fn names(listing: &str) -> Vec<String> {
    listing
        .lines()
        .map(|line| line.trim_matches(|c: char| c.is_whitespace() || c == '\0'))
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Rows of a `--list --verbose` listing: a localized header, then the name, state and version of each
/// distribution, the default one marked with `*`.
fn listed(listing: &str) -> Vec<Listed> {
    listing
        .lines()
        .skip(1)
        .filter_map(|line| {
            let line = line.trim_matches(|c: char| c.is_whitespace() || c == '\0');
            let (is_default, row) = match line.strip_prefix('*') {
                Some(row) => (true, row),
                None => (false, line),
            };
            let mut fields = row.split_whitespace();
            let name = fields.next()?;
            let version = fields.next_back()?.parse().ok()?;

            Some(Listed {
                name: name.to_owned(),
                version,
                is_default,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rejection_is_what_the_agent_wrote_or_its_message() {
        let stdout = r#"{"reason":"unknown key","unknown_keys":[{"key":"debgu","suggestion":"debug"}]}"#;
        let stderr = "wsl: A localhost proxy configuration was detected but not mirrored into WSL.\r\n\
                      Error: the engine rejects its configuration: unknown key debgu\n";

        let written = rejection(&format!("{stdout}\n"), stderr);
        assert_eq!(written.unknown_keys[0].suggestion.as_deref(), Some("debug"));
        assert_eq!(
            rejection("", stderr).reason,
            "the engine rejects its configuration: unknown key debgu"
        );
    }

    #[test]
    fn decodes_utf8_and_utf16_output() {
        let utf16: Vec<u8> = "contr\r\n".encode_utf16().flat_map(u16::to_le_bytes).collect();

        assert_eq!(decode(b"contr\n"), "contr\n");
        assert_eq!(decode(&utf16), "contr\r\n");
    }

    #[test]
    fn lists_distributions_with_their_version() {
        let listing = "  NAME            STATE           VERSION\r\n\
                       * Ubuntu-24.04    Running         2\r\n\
                       \x20 Legacy          Stopped         1\r\n\
                       \r\n";

        assert_eq!(
            listed(listing),
            [
                Listed {
                    name: "Ubuntu-24.04".to_owned(),
                    version: 2,
                    is_default: true,
                },
                Listed {
                    name: "Legacy".to_owned(),
                    version: 1,
                    is_default: false,
                },
            ]
        );
    }

    #[test]
    fn lists_distribution_names() {
        assert_eq!(
            names("Ubuntu\r\ncontr\r\n\r\ncontr-data\r\n"),
            ["Ubuntu", "contr", "contr-data"]
        );
    }
}
