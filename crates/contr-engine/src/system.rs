//! Containers Contr runs for itself, such as the Kubernetes server, and commands it runs in them.

use std::collections::HashMap;

use bollard::models as docker;
use bollard::query_parameters::CreateContainerOptions;
use bytes::Bytes;
use contr_api::containers::v1::ExecStart;
use futures_util::{Stream, StreamExt};
use tokio::io::AsyncWriteExt;

use crate::exec::ExecOutput;
use crate::{Engine, EngineError};

/// A privileged container that restarts with the engine unless stopped.
#[derive(Clone, Debug, Default)]
pub struct SystemContainer {
    pub name: String,
    pub image: String,
    pub hostname: String,
    pub command: Vec<String>,
    /// Named volumes by the directory each is mounted at.
    pub volumes: Vec<(String, String)>,
    /// Directories mounted as memory file systems.
    pub tmpfs: Vec<String>,
    /// Container port published on the same port of the engine's loopback interface.
    pub loopback_port: Option<u16>,
    pub labels: HashMap<String, String>,
}

/// Exit code and output of a command run to completion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl RunOutput {
    /// Standard error as text, for failure messages.
    pub fn error(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_owned()
    }
}

impl Engine {
    /// Creates `container` without starting it and returns its ID.
    pub async fn create_system_container(&self, container: &SystemContainer) -> Result<String, EngineError> {
        let port = container.loopback_port.map(|port| format!("{port}/tcp"));
        let body = docker::ContainerCreateBody {
            image: Some(container.image.clone()),
            hostname: Some(container.hostname.clone()).filter(|hostname| !hostname.is_empty()),
            cmd: Some(container.command.clone()),
            labels: Some(container.labels.clone()),
            exposed_ports: port.clone().map(|port| vec![port]),
            host_config: Some(docker::HostConfig {
                privileged: Some(true),
                restart_policy: Some(docker::RestartPolicy {
                    name: Some(docker::RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: None,
                }),
                mounts: Some(
                    container
                        .volumes
                        .iter()
                        .map(|(volume, target)| docker::Mount {
                            typ: Some(docker::MountType::VOLUME),
                            source: Some(volume.clone()),
                            target: Some(target.clone()),
                            ..docker::Mount::default()
                        })
                        .collect(),
                ),
                tmpfs: Some(container.tmpfs.iter().map(|dir| (dir.clone(), String::new())).collect()),
                port_bindings: port.zip(container.loopback_port).map(|(port, host_port)| {
                    HashMap::from([(
                        port,
                        Some(vec![docker::PortBinding {
                            host_ip: Some("127.0.0.1".to_owned()),
                            host_port: Some(host_port.to_string()),
                        }]),
                    )])
                }),
                ..docker::HostConfig::default()
            }),
            ..docker::ContainerCreateBody::default()
        };
        let options = CreateContainerOptions {
            name: Some(container.name.clone()),
            platform: String::new(),
        };

        Ok(self.docker().create_container(Some(options), body).await?.id)
    }

    /// Runs `command` in the running `container` without a terminal, feeding it `stdin` when given, and returns once
    /// it exits.
    pub async fn run_in(
        &self,
        container: &str,
        command: &[&str],
        stdin: Option<impl Stream<Item = Result<Bytes, EngineError>> + Send + Unpin>,
    ) -> Result<RunOutput, EngineError> {
        let start = ExecStart {
            container_id: container.to_owned(),
            command: command.iter().map(|part| (*part).to_owned()).collect(),
            attach_stdin: stdin.is_some(),
            ..ExecStart::default()
        };
        let mut session = self.exec(&start).await?;

        // Stdin closes whatever happens to the input, so the command ends and the output stream with it.
        let feed = async {
            let closed = |error: std::io::Error| EngineError::Engine(format!("cannot write to the command: {error}"));
            let mut fed = Ok(());
            if let Some(mut stdin) = stdin {
                while let Some(chunk) = stdin.next().await {
                    let written = match chunk {
                        Ok(chunk) => session.input.write_all(&chunk).await.map_err(closed),
                        Err(error) => Err(error),
                    };
                    if written.is_err() {
                        fed = written;
                        break;
                    }
                }
            }
            let shut = session.input.shutdown().await.map_err(closed);
            fed.and(shut)
        };
        let collect = async {
            let mut output = RunOutput::default();
            while let Some(chunk) = session.output.next().await {
                match chunk? {
                    ExecOutput::Stdout(bytes) => output.stdout.extend_from_slice(&bytes),
                    ExecOutput::Stderr(bytes) => output.stderr.extend_from_slice(&bytes),
                }
            }
            Ok::<RunOutput, EngineError>(output)
        };
        let (written, collected) = tokio::join!(feed, collect);
        let mut output = collected?;
        written?;
        output.exit_code = self.exec_exit_code(&session.id).await?;

        Ok(output)
    }

    /// The `docker save` archive of `images`.
    pub fn export_images(&self, images: &[String]) -> impl Stream<Item = Result<Bytes, EngineError>> + Send + use<> {
        let names: Vec<&str> = images.iter().map(String::as_str).collect();

        self.docker()
            .export_images(&names)
            .map(|chunk| chunk.map_err(EngineError::from))
    }
}
