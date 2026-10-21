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
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn reports_a_held_port_until_it_is_released() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        assert!(in_use(port, Transport::Tcp));
        drop(listener);
        // A process another test spawns holds a copy of the listener until it executes its program.
        let deadline = Instant::now() + Duration::from_secs(2);
        while in_use(port, Transport::Tcp) {
            assert!(
                Instant::now() < deadline,
                "port {port} stayed held after its listener closed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
