//! Tells the backend which TCP ports the engine publishes.

use std::collections::BTreeSet;
use std::sync::Arc;

use contr_api::containers::v1::{Container, Protocol};
use contr_backend::{Backend, PublishedPort};
use futures_util::StreamExt;

use crate::slot::EngineSlot;

/// Hands `backend` the published TCP ports after every container change, and none while the engine is not running.
pub(crate) fn spawn(slot: &EngineSlot, backend: Arc<dyn Backend>) {
    let mut engine = slot.subscribe();
    tokio::spawn(async move {
        let mut published = BTreeSet::new();
        loop {
            let handle = engine.borrow_and_update().clone();
            if let Some(handle) = handle {
                let mut changes = handle.cache.watch_containers();
                while changes.next().await.is_some() {
                    publish(&*backend, &mut published, tcp_ports(&handle.cache.containers()));
                }
            }
            publish(&*backend, &mut published, BTreeSet::new());
            if engine.changed().await.is_err() {
                return;
            }
        }
    });
}

fn publish(backend: &dyn Backend, published: &mut BTreeSet<PublishedPort>, ports: BTreeSet<PublishedPort>) {
    if *published != ports {
        backend.publish_ports(&ports);
        *published = ports;
    }
}

fn tcp_ports(containers: &[Container]) -> BTreeSet<PublishedPort> {
    containers
        .iter()
        .flat_map(|container| &container.ports)
        .filter(|mapping| mapping.protocol() == Protocol::Tcp)
        .filter_map(|mapping| {
            Some(PublishedPort {
                address: mapping.host_ip.parse().ok()?,
                port: u16::try_from(mapping.host_port).ok().filter(|port| *port != 0)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use contr_api::containers::v1::PortMapping;

    use super::*;

    fn mapping(host_ip: &str, host_port: u32, protocol: Protocol) -> PortMapping {
        PortMapping {
            container_port: 80,
            host_port,
            host_ip: host_ip.to_owned(),
            protocol: protocol.into(),
        }
    }

    #[test]
    fn keeps_published_tcp_ports_only() {
        let web = Container {
            ports: vec![
                mapping("0.0.0.0", 8080, Protocol::Tcp),
                mapping("::", 8080, Protocol::Tcp),
                mapping("", 0, Protocol::Tcp),
                mapping("0.0.0.0", 5353, Protocol::Udp),
            ],
            ..Container::default()
        };
        let api = Container {
            ports: vec![mapping("127.0.0.1", 6443, Protocol::Tcp)],
            ..Container::default()
        };

        let ports: Vec<_> = tcp_ports(&[web, api]).into_iter().collect();

        assert_eq!(
            ports,
            [
                PublishedPort {
                    address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 8080
                },
                PublishedPort {
                    address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: 6443
                },
                PublishedPort {
                    address: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                    port: 8080
                },
            ]
        );
    }
}
