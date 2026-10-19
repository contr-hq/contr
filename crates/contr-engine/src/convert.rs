//! Conversions from Docker Engine API models to `contr-api` messages.

use std::collections::HashMap;
use std::str::FromStr;

use bollard::models as docker;
use contr_api::containers::v1 as containers;
use contr_api::images::v1 as images;
use contr_api::networks::v1 as networks;
use contr_api::volumes::v1 as volumes;
use prost_types::Timestamp;

pub(crate) fn container(summary: docker::ContainerSummary) -> containers::Container {
    containers::Container {
        id: summary.id.unwrap_or_default(),
        names: summary
            .names
            .unwrap_or_default()
            .into_iter()
            .map(|name| name.trim_start_matches('/').to_owned())
            .collect(),
        image: summary.image.unwrap_or_default(),
        image_id: summary.image_id.unwrap_or_default(),
        command: summary.command.unwrap_or_default(),
        created_at: summary.created.map(unix_timestamp),
        state: summary
            .state
            .map_or(containers::ContainerState::Unspecified, container_state)
            .into(),
        status: summary.status.unwrap_or_default(),
        ports: summary.ports.unwrap_or_default().into_iter().map(port).collect(),
        labels: summary.labels.unwrap_or_default(),
    }
}

fn container_state(state: docker::ContainerSummaryStateEnum) -> containers::ContainerState {
    use containers::ContainerState as State;
    use docker::ContainerSummaryStateEnum as Engine;

    match state {
        Engine::EMPTY => State::Unspecified,
        Engine::CREATED => State::Created,
        Engine::RUNNING => State::Running,
        Engine::PAUSED => State::Paused,
        Engine::RESTARTING => State::Restarting,
        Engine::EXITED => State::Exited,
        Engine::REMOVING => State::Removing,
        Engine::DEAD => State::Dead,
        Engine::STOPPING => State::Stopping,
    }
}

fn port(port: docker::PortSummary) -> containers::PortMapping {
    let protocol = match port.typ {
        Some(docker::PortSummaryTypeEnum::TCP) => containers::Protocol::Tcp,
        Some(docker::PortSummaryTypeEnum::UDP) => containers::Protocol::Udp,
        Some(docker::PortSummaryTypeEnum::SCTP) => containers::Protocol::Sctp,
        Some(docker::PortSummaryTypeEnum::EMPTY) | None => containers::Protocol::Unspecified,
    };

    containers::PortMapping {
        container_port: port.private_port.into(),
        host_port: port.public_port.map_or(0, u32::from),
        host_ip: port.ip.unwrap_or_default(),
        protocol: protocol.into(),
    }
}

pub(crate) fn container_details(inspect: &docker::ContainerInspectResponse) -> containers::ContainerDetails {
    let state = inspect.state.as_ref();
    let config = inspect.config.as_ref();
    let health = state
        .and_then(|state| state.health.as_ref())
        .and_then(|health| health.status)
        .map_or(containers::HealthStatus::Unspecified, health_status);

    containers::ContainerDetails {
        started_at: state.and_then(|state| state.started_at.as_deref()).and_then(rfc3339),
        finished_at: state.and_then(|state| state.finished_at.as_deref()).and_then(rfc3339),
        exit_code: state
            .and_then(|state| state.exit_code)
            .and_then(|code| i32::try_from(code).ok())
            .unwrap_or_default(),
        restart_count: inspect
            .restart_count
            .and_then(|count| u32::try_from(count).ok())
            .unwrap_or_default(),
        tty: config.and_then(|config| config.tty).unwrap_or_default(),
        health: health.into(),
        working_dir: config.and_then(|config| config.working_dir.clone()).unwrap_or_default(),
        env: config.and_then(|config| config.env.clone()).unwrap_or_default(),
        mounts: inspect.mounts.iter().flatten().map(mount).collect(),
    }
}

fn health_status(status: docker::HealthStatusEnum) -> containers::HealthStatus {
    use containers::HealthStatus as Health;
    use docker::HealthStatusEnum as Engine;

    match status {
        Engine::EMPTY => Health::Unspecified,
        Engine::NONE => Health::None,
        Engine::STARTING => Health::Starting,
        Engine::HEALTHY => Health::Healthy,
        Engine::UNHEALTHY => Health::Unhealthy,
    }
}

fn mount(mount: &docker::MountPoint) -> containers::Mount {
    use containers::MountKind as Kind;

    let kind = match mount.typ.as_deref() {
        Some("bind") => Kind::Bind,
        Some("volume") => Kind::Volume,
        Some("tmpfs") => Kind::Tmpfs,
        Some("npipe") => Kind::Npipe,
        Some("cluster") => Kind::Cluster,
        Some("image") => Kind::Image,
        _ => Kind::Unspecified,
    };

    containers::Mount {
        kind: kind.into(),
        name: mount.name.clone().unwrap_or_default(),
        source: mount.source.clone().unwrap_or_default(),
        destination: mount.destination.clone().unwrap_or_default(),
        read_only: !mount.rw.unwrap_or(true),
    }
}

pub(crate) fn image(summary: docker::ImageSummary) -> images::Image {
    images::Image {
        id: summary.id,
        repo_tags: summary.repo_tags,
        repo_digests: summary.repo_digests,
        created_at: Some(unix_timestamp(summary.created)),
        size_bytes: u64::try_from(summary.size).unwrap_or_default(),
        labels: summary.labels,
        containers: summary.containers,
    }
}

pub(crate) fn volume(volume: docker::Volume) -> volumes::Volume {
    let scope = match volume.scope {
        Some(docker::VolumeScopeEnum::LOCAL) => volumes::VolumeScope::Local,
        Some(docker::VolumeScopeEnum::GLOBAL) => volumes::VolumeScope::Global,
        Some(docker::VolumeScopeEnum::EMPTY) | None => volumes::VolumeScope::Unspecified,
    };

    volumes::Volume {
        name: volume.name,
        driver: volume.driver,
        mountpoint: volume.mountpoint,
        created_at: volume.created_at.as_deref().and_then(rfc3339),
        labels: volume.labels,
        options: volume.options,
        scope: scope.into(),
    }
}

pub(crate) fn network(network: docker::Network) -> networks::Network {
    NetworkFields {
        id: network.id,
        name: network.name,
        driver: network.driver,
        scope: network.scope,
        internal: network.internal,
        attachable: network.attachable,
        enable_ipv6: network.enable_ipv6,
        created: network.created,
        labels: network.labels,
        ipam: network.ipam,
    }
    .into_message()
}

pub(crate) fn inspected_network(network: docker::NetworkInspect) -> networks::Network {
    NetworkFields {
        id: network.id,
        name: network.name,
        driver: network.driver,
        scope: network.scope,
        internal: network.internal,
        attachable: network.attachable,
        enable_ipv6: network.enable_ipv6,
        created: network.created,
        labels: network.labels,
        ipam: network.ipam,
    }
    .into_message()
}

struct NetworkFields {
    id: Option<String>,
    name: Option<String>,
    driver: Option<String>,
    scope: Option<String>,
    internal: Option<bool>,
    attachable: Option<bool>,
    enable_ipv6: Option<bool>,
    created: Option<String>,
    labels: Option<HashMap<String, String>>,
    ipam: Option<docker::Ipam>,
}

impl NetworkFields {
    fn into_message(self) -> networks::Network {
        let scope = match self.scope.as_deref() {
            Some("local") => networks::NetworkScope::Local,
            Some("global") => networks::NetworkScope::Global,
            Some("swarm") => networks::NetworkScope::Swarm,
            _ => networks::NetworkScope::Unspecified,
        };
        let subnets = self
            .ipam
            .and_then(|ipam| ipam.config)
            .unwrap_or_default()
            .into_iter()
            .map(|config| networks::Subnet {
                cidr: config.subnet.unwrap_or_default(),
                gateway: config.gateway.unwrap_or_default(),
            })
            .collect();

        networks::Network {
            id: self.id.unwrap_or_default(),
            name: self.name.unwrap_or_default(),
            driver: self.driver.unwrap_or_default(),
            scope: scope.into(),
            internal: self.internal.unwrap_or_default(),
            attachable: self.attachable.unwrap_or_default(),
            ipv6_enabled: self.enable_ipv6.unwrap_or_default(),
            created_at: self.created.as_deref().and_then(rfc3339),
            labels: self.labels.unwrap_or_default(),
            subnets,
        }
    }
}

pub(crate) fn unix_timestamp(seconds: i64) -> Timestamp {
    Timestamp { seconds, nanos: 0 }
}

/// Parses an engine timestamp; the engine writes year 1 for times that never happened, which maps to `None`.
pub(crate) fn rfc3339(text: &str) -> Option<Timestamp> {
    Timestamp::from_str(text).ok().filter(|timestamp| timestamp.seconds > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn treats_the_zero_engine_time_as_absent() {
        assert_eq!(rfc3339("0001-01-01T00:00:00Z"), None);
        assert_eq!(
            rfc3339("2025-10-09T08:53:20.5Z").map(|t| (t.seconds, t.nanos)),
            Some((1_760_000_000, 500_000_000))
        );
    }

    #[test]
    fn converts_a_container_summary() {
        let summary: docker::ContainerSummary = serde_json::from_value(serde_json::json!({
            "Id": "c0ffee",
            "Names": ["/web"],
            "Image": "nginx",
            "State": "running",
            "Status": "Up 2 minutes",
            "Created": 1_760_000_000,
            "Ports": [{ "IP": "0.0.0.0", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp" }],
        }))
        .unwrap();

        let container = container(summary);

        assert_eq!(container.names, ["web"]);
        assert_eq!(container.state(), containers::ContainerState::Running);
        assert_eq!(container.ports[0].host_port, 8080);
        assert_eq!(container.ports[0].protocol(), containers::Protocol::Tcp);
    }
}
