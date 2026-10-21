use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use contr_api::compose::v1::Action;
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_stream::wrappers::LinesStream;

use crate::project::Declared;

/// File name of the Compose CLI plugin.
#[cfg(windows)]
const PLUGIN: &str = "docker-compose.exe";
#[cfg(not(windows))]
const PLUGIN: &str = "docker-compose";

/// Directories the Docker CLI looks in for plugins after the user's own, in its order.
#[cfg(not(windows))]
const SYSTEM_PLUGIN_DIRS: [&str; 4] = [
    "/usr/local/lib/docker/cli-plugins",
    "/usr/local/libexec/docker/cli-plugins",
    "/usr/lib/docker/cli-plugins",
    "/usr/libexec/docker/cli-plugins",
];

/// The Compose CLI, run on its own against one engine.
#[derive(Clone, Debug)]
pub struct Compose {
    program: PathBuf,
    docker_host: String,
}

/// A line of Compose's output, then its exit code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    Line(String),
    Exit(i32),
}

/// The project an action runs on.
#[derive(Clone, Debug, Default)]
pub struct Target {
    pub project: String,
    pub working_dir: String,
    pub config_files: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ComposeError {
    #[error("{0} needs the project's Compose files, and none are known")]
    NoConfigFiles(&'static str),
    #[error("{0} is not an action")]
    UnknownAction(i32),
    #[error("cannot run Compose")]
    Spawn(#[source] io::Error),
}

impl Compose {
    /// Compose at `program`, run against the engine at `docker_host`.
    pub fn new(program: PathBuf, docker_host: String) -> Self {
        Self { program, docker_host }
    }

    /// The Compose plugin: in `bundle` when given, else where the Docker CLI looks for plugins, the user's own
    /// directory first.
    pub fn find(bundle: Option<&Path>) -> Option<PathBuf> {
        let user = env::var_os("DOCKER_CONFIG")
            .map(PathBuf::from)
            .or_else(|| env::home_dir().map(|home| home.join(".docker")))
            .map(|config| config.join("cli-plugins"));
        #[cfg(not(windows))]
        let system = SYSTEM_PLUGIN_DIRS.map(PathBuf::from);
        #[cfg(windows)]
        let system: [PathBuf; 0] = [];

        bundle
            .map(Path::to_path_buf)
            .into_iter()
            .chain(user)
            .chain(system)
            .map(|dir| dir.join(PLUGIN))
            .find(|program| program.is_file())
    }

    /// What `config_files` declare, or Compose's reason it cannot read them.
    pub async fn config(&self, config_files: &[String]) -> Result<Declared, String> {
        let output = self
            .command()
            .args(file_args(config_files))
            .args(["config", "--format", "json"])
            .output()
            .await
            .map_err(|error| format!("cannot run Compose: {error}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }

        Declared::parse(&String::from_utf8_lossy(&output.stdout), config_files)
            .map_err(|error| format!("Compose printed a configuration that is not JSON: {error}"))
    }

    /// Runs `action` on `services` of `target`, or on all of them when none are named, and streams Compose's output.
    pub fn run(
        &self,
        target: &Target,
        action: Action,
        services: &[String],
        remove_volumes: bool,
    ) -> Result<BoxStream<'static, io::Result<Output>>, ComposeError> {
        let (verb, needs_files) = match action {
            Action::Up => ("up", true),
            Action::Pull => ("pull", true),
            Action::Start => ("start", false),
            Action::Stop => ("stop", false),
            Action::Restart => ("restart", false),
            Action::Down => ("down", false),
            Action::Unspecified => return Err(ComposeError::UnknownAction(action.into())),
        };
        if needs_files && target.config_files.is_empty() {
            return Err(ComposeError::NoConfigFiles(verb));
        }

        let mut command = self.command();
        command.args(["--project-name", &target.project]);
        if needs_files {
            command
                .args(["--project-directory", &target.working_dir])
                .args(file_args(&target.config_files));
        }
        command.arg(verb);
        match action {
            Action::Up => {
                command.arg("--detach").args(services);
            }
            Action::Down => {
                if remove_volumes {
                    command.arg("--volumes");
                }
            }
            _ => {
                command.args(services);
            }
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(ComposeError::Spawn)?;

        let stdout = child
            .stdout
            .take()
            .map(|out| LinesStream::new(BufReader::new(out).lines()));
        let stderr = child
            .stderr
            .take()
            .map(|err| LinesStream::new(BufReader::new(err).lines()));
        let lines = stream::select(stream::iter(stdout).flatten(), stream::iter(stderr).flatten())
            .map(|line| line.map(Output::Line));
        let exit = stream::once(async move {
            let status = child.wait().await?;
            Ok(Output::Exit(status.code().unwrap_or(-1)))
        });

        Ok(lines.chain(exit).boxed())
    }

    /// Compose with plain output, against this engine, outside any directory holding Compose files.
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(["--ansi", "never", "--progress", "plain"])
            .env("DOCKER_HOST", &self.docker_host)
            .env_remove("DOCKER_CONTEXT")
            .env_remove("COMPOSE_FILE")
            .env_remove("COMPOSE_PROJECT_NAME")
            .current_dir(env::temp_dir())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn file_args(config_files: &[String]) -> Vec<String> {
    config_files
        .iter()
        .flat_map(|file| ["--file".to_owned(), file.clone()])
        .collect()
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use futures_util::TryStreamExt;

    use super::*;

    /// A stand-in for Compose that prints its arguments and `DOCKER_HOST`, then exits with 3.
    fn fake_compose(dir: &Path) -> PathBuf {
        let program = dir.join("docker-compose");
        std::fs::write(
            &program,
            "#!/bin/sh\necho \"args: $*\"\necho \"host: $DOCKER_HOST\" >&2\nexit 3\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    fn target() -> Target {
        Target {
            project: "shop".to_owned(),
            working_dir: "/src/shop".to_owned(),
            config_files: vec!["/src/shop/compose.yaml".to_owned()],
        }
    }

    #[tokio::test]
    async fn up_runs_on_the_project_files_and_reports_the_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let compose = Compose::new(fake_compose(dir.path()), "unix:///run/engine.sock".to_owned());

        let mut output: Vec<Output> = compose
            .run(&target(), Action::Up, &["api".to_owned()], false)
            .unwrap()
            .try_collect()
            .await
            .unwrap();

        assert_eq!(output.pop(), Some(Output::Exit(3)));
        output.sort_by_key(|line| format!("{line:?}"));
        assert_eq!(
            output,
            [
                Output::Line(
                    "args: --ansi never --progress plain --project-name shop --project-directory /src/shop --file \
                     /src/shop/compose.yaml up --detach api"
                        .to_owned()
                ),
                Output::Line("host: unix:///run/engine.sock".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn down_needs_only_the_project_name() {
        let dir = tempfile::tempdir().unwrap();
        let compose = Compose::new(fake_compose(dir.path()), String::new());
        let named_only = Target {
            project: "shop".to_owned(),
            ..Target::default()
        };

        let output: Vec<Output> = compose
            .run(&named_only, Action::Down, &["ignored".to_owned()], true)
            .unwrap()
            .try_collect()
            .await
            .unwrap();

        assert!(output.contains(&Output::Line(
            "args: --ansi never --progress plain --project-name shop down --volumes".to_owned()
        )));
        assert!(matches!(
            compose.run(&named_only, Action::Pull, &[], false),
            Err(ComposeError::NoConfigFiles("pull"))
        ));
    }

    #[test]
    fn finds_the_plugin_in_the_bundle_first() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_compose(dir.path());

        assert_eq!(Compose::find(Some(dir.path())), Some(program));
    }
}
