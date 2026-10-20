//! Engine container definitions built from `CreateContainer` requests.

use std::collections::HashMap;

use bollard::models as docker;
use contr_api::containers::v1::{
    CreateContainerRequest, MountKind, MountSpec, PortPublication, Protocol, RestartMode, RestartPolicy,
};

use crate::EngineError;

/// The engine's definition of the container `request` describes.
pub(crate) fn definition(request: &CreateContainerRequest) -> Result<docker::ContainerCreateBody, EngineError> {
    if request.image.is_empty() {
        return Err(EngineError::InvalidArgument("an image is required".to_owned()));
    }
    let mut port_bindings: docker::PortMap = HashMap::new();
    for publication in &request.ports {
        let (key, binding) = port(publication)?;
        port_bindings
            .entry(key)
            .or_insert_with(|| Some(Vec::new()))
            .get_or_insert_default()
            .push(binding);
    }
    let mounts = request.mounts.iter().map(mount).collect::<Result<Vec<_>, _>>()?;
    let network = text(&request.network);

    Ok(docker::ContainerCreateBody {
        image: Some(request.image.clone()),
        cmd: list(&request.command),
        entrypoint: list(&request.entrypoint),
        env: list(&request.env),
        labels: (!request.labels.is_empty()).then(|| request.labels.clone()),
        working_dir: text(&request.working_dir),
        user: text(&request.user),
        tty: Some(request.interactive),
        open_stdin: Some(request.interactive),
        exposed_ports: (!port_bindings.is_empty()).then(|| port_bindings.keys().cloned().collect()),
        host_config: Some(docker::HostConfig {
            port_bindings: (!port_bindings.is_empty()).then_some(port_bindings),
            mounts: (!mounts.is_empty()).then_some(mounts),
            restart_policy: request.restart_policy.map(restart_policy),
            auto_remove: Some(request.auto_remove),
            network_mode: network,
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn port(publication: &PortPublication) -> Result<(String, docker::PortBinding), EngineError> {
    let container_port = u16::try_from(publication.container_port)
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| EngineError::InvalidArgument(format!("{} is not a port", publication.container_port)))?;
    let host_port = u16::try_from(publication.host_port)
        .map_err(|_| EngineError::InvalidArgument(format!("{} is not a port", publication.host_port)))?;
    let protocol = match publication.protocol() {
        Protocol::Unspecified | Protocol::Tcp => "tcp",
        Protocol::Udp => "udp",
        Protocol::Sctp => "sctp",
    };

    Ok((
        format!("{container_port}/{protocol}"),
        docker::PortBinding {
            host_ip: text(&publication.host_ip),
            host_port: (host_port != 0).then(|| host_port.to_string()),
        },
    ))
}

fn mount(spec: &MountSpec) -> Result<docker::Mount, EngineError> {
    if spec.destination.is_empty() {
        return Err(EngineError::InvalidArgument(
            "every mount needs a destination".to_owned(),
        ));
    }
    let typ = match spec.kind() {
        MountKind::Bind if spec.source.is_empty() => {
            return Err(EngineError::InvalidArgument(format!(
                "the bind mount at {} needs a source",
                spec.destination
            )));
        }
        MountKind::Bind => docker::MountType::BIND,
        MountKind::Volume => docker::MountType::VOLUME,
        MountKind::Tmpfs => docker::MountType::TMPFS,
        other => {
            return Err(EngineError::InvalidArgument(format!(
                "{} mounts cannot be created",
                other.as_str_name()
            )));
        }
    };

    Ok(docker::Mount {
        typ: Some(typ),
        source: text(&spec.source),
        target: Some(spec.destination.clone()),
        read_only: Some(spec.read_only),
        ..Default::default()
    })
}

fn restart_policy(policy: RestartPolicy) -> docker::RestartPolicy {
    use docker::RestartPolicyNameEnum as Name;

    let name = match policy.mode() {
        RestartMode::Unspecified | RestartMode::No => Name::NO,
        RestartMode::Always => Name::ALWAYS,
        RestartMode::UnlessStopped => Name::UNLESS_STOPPED,
        RestartMode::OnFailure => Name::ON_FAILURE,
    };
    let retries = (name == Name::ON_FAILURE).then(|| i64::from(policy.max_retries));

    docker::RestartPolicy {
        name: Some(name),
        maximum_retry_count: retries,
    }
}

fn list(values: &[String]) -> Option<Vec<String>> {
    (!values.is_empty()).then(|| values.to_vec())
}

fn text(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> CreateContainerRequest {
        CreateContainerRequest {
            image: "nginx:1.27-alpine".to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn publishes_ports_and_mounts_folders() {
        let request = CreateContainerRequest {
            ports: vec![
                PortPublication {
                    container_port: 80,
                    host_port: 8082,
                    ..Default::default()
                },
                PortPublication {
                    container_port: 80,
                    protocol: Protocol::Tcp.into(),
                    host_ip: "127.0.0.1".to_owned(),
                    ..Default::default()
                },
            ],
            mounts: vec![MountSpec {
                kind: MountKind::Bind.into(),
                source: r"C:\src\site".to_owned(),
                destination: "/usr/share/nginx/html".to_owned(),
                read_only: true,
            }],
            network: "shop".to_owned(),
            auto_remove: true,
            ..request()
        };

        let definition = definition(&request).unwrap();
        let host = definition.host_config.unwrap();

        assert_eq!(definition.exposed_ports, Some(vec!["80/tcp".to_owned()]));
        let bindings = host.port_bindings.unwrap()["80/tcp"].clone().unwrap();
        assert_eq!(bindings[0].host_port.as_deref(), Some("8082"));
        assert_eq!(bindings[1].host_port, None);
        assert_eq!(bindings[1].host_ip.as_deref(), Some("127.0.0.1"));
        let mount = &host.mounts.unwrap()[0];
        assert_eq!(mount.typ, Some(docker::MountType::BIND));
        assert_eq!(mount.source.as_deref(), Some(r"C:\src\site"));
        assert_eq!(mount.read_only, Some(true));
        assert_eq!(host.network_mode.as_deref(), Some("shop"));
        assert_eq!(host.auto_remove, Some(true));
        assert_eq!(definition.cmd, None);
    }

    #[test]
    fn retries_only_on_failure() {
        let policy = |mode: RestartMode| {
            restart_policy(RestartPolicy {
                mode: mode.into(),
                max_retries: 3,
            })
        };

        assert_eq!(policy(RestartMode::OnFailure).maximum_retry_count, Some(3));
        assert_eq!(policy(RestartMode::Always).maximum_retry_count, None);
        assert_eq!(
            policy(RestartMode::Unspecified).name,
            Some(docker::RestartPolicyNameEnum::NO)
        );
    }

    #[test]
    fn rejects_incomplete_definitions() {
        let invalid =
            |request: CreateContainerRequest| matches!(definition(&request), Err(EngineError::InvalidArgument(_)));

        assert!(invalid(CreateContainerRequest::default()));
        assert!(invalid(CreateContainerRequest {
            ports: vec![PortPublication::default()],
            ..request()
        }));
        assert!(invalid(CreateContainerRequest {
            mounts: vec![MountSpec {
                kind: MountKind::Bind.into(),
                destination: "/w".to_owned(),
                ..Default::default()
            }],
            ..request()
        }));
        assert!(invalid(CreateContainerRequest {
            mounts: vec![MountSpec {
                kind: MountKind::Npipe.into(),
                source: "x".to_owned(),
                destination: "/w".to_owned(),
                ..Default::default()
            }],
            ..request()
        }));
    }
}
