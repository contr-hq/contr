//! Ports of this computer that a program already holds.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpListener, UdpSocket};

/// Transport of a port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Tcp,
    Udp,
}

/// Addresses where a published port would listen: every IPv4 address and the loopback addresses that
/// `localhost` resolves to.
const ADDRESSES: [IpAddr; 3] = [
    IpAddr::V4(Ipv4Addr::UNSPECIFIED),
    IpAddr::V4(Ipv4Addr::LOCALHOST),
    IpAddr::V6(Ipv6Addr::LOCALHOST),
];

/// Whether a program holds `port` on one of the addresses a published port would use, judged by trying to bind it.
pub fn in_use(port: u16, transport: Transport) -> bool {
    ADDRESSES.iter().any(|&address| {
        let bound = match transport {
            Transport::Tcp => TcpListener::bind((address, port)).map(drop),
            Transport::Udp => UdpSocket::bind((address, port)).map(drop),
        };
        bound.is_err_and(|error| held(&error))
    })
}

/// Windows reports a port another program holds exclusively, or one it reserves, as access denied; Unix reports
/// privileged ports that way, which an engine may still publish.
fn held(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::AddrInUse || (cfg!(windows) && error.kind() == io::ErrorKind::PermissionDenied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_held_port_until_it_is_released() {
        let (port, listeners) = loop {
            let ipv4 = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
            let port = ipv4.local_addr().unwrap().port();
            // Another program may hold the same port number on the IPv6 loopback address.
            match TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
                Ok(ipv6) => break (port, vec![ipv4, ipv6]),
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => {}
                Err(_) => break (port, vec![ipv4]),
            }
        };

        assert!(in_use(port, Transport::Tcp));
        drop(listeners);
        assert!(!in_use(port, Transport::Tcp));
    }
}
