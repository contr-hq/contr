use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::time::Duration;

use contr_backend::PublishedPort;
use contr_mux::{MuxError, Opener};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{AbortHandle, JoinSet};

/// Pause after a failed accept, which fails again at once while the process is out of sockets.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// Listens on this machine at each port in `ports`, at the address the engine bound in the guest, and relays every
/// connection to that port on the guest's loopback interface. A port another program holds here, as WSL's own
/// localhost forwarding may, is tried again at the next change. Dropping the future closes every listener and
/// connection.
pub(crate) async fn forward(mut ports: watch::Receiver<BTreeSet<PublishedPort>>, opener: Opener) {
    let mut listeners = JoinSet::new();
    let mut serving: HashMap<PublishedPort, AbortHandle> = HashMap::new();
    loop {
        let wanted = ports.borrow_and_update().clone();
        serving.retain(|port, listener| {
            let keep = wanted.contains(port);
            if !keep {
                listener.abort();
            }
            keep
        });
        for port in wanted {
            if serving.contains_key(&port) {
                continue;
            }
            let address = SocketAddr::new(port.address, port.port);
            match TcpListener::bind(address).await {
                Ok(listener) => {
                    tracing::debug!(%address, "serving a published port");
                    serving.insert(port, listeners.spawn(serve(listener, port.port, opener.clone())));
                }
                Err(error) => tracing::debug!(%error, %address, "cannot serve a published port"),
            }
        }

        tokio::select! {
            changed = ports.changed() => if changed.is_err() { return },
            Some(_) = listeners.join_next(), if !listeners.is_empty() => {}
        }
    }
}

async fn serve(listener: TcpListener, port: u16, opener: Opener) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((connection, _)) => {
                    connections.spawn(relay(connection, port, opener.clone()));
                }
                Err(error) => {
                    tracing::debug!(%error, port, "cannot accept a connection");
                    tokio::time::sleep(ACCEPT_RETRY).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

async fn relay(mut connection: TcpStream, port: u16, opener: Opener) {
    let relayed = async {
        let mut stream = opener.open_guest_tcp(port).await?;
        tokio::io::copy_bidirectional(&mut connection, &mut stream).await?;
        Ok::<_, MuxError>(())
    };
    if let Err(error) = relayed.await {
        tracing::debug!(%error, port, "published port connection ended");
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::net::{IpAddr, Ipv4Addr};

    use contr_mux::{Channel, MuxStream, Role};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
    use tokio::time::{Instant, sleep};

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    /// Stands in for the guest: answers a `GuestTcp` stream with the port it names.
    async fn report_port(channel: Channel, mut stream: MuxStream) {
        assert_eq!(channel, Channel::GuestTcp);
        let port = contr_mux::read_port(&mut stream).await.unwrap();
        stream.write_all(format!("guest port {port}").as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    }

    async fn free_port() -> PublishedPort {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();

        PublishedPort {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: listener.local_addr().unwrap().port(),
        }
    }

    /// Connects once `port` accepts, and reads what the far end sends.
    async fn read_from(port: PublishedPort) -> io::Result<String> {
        let deadline = Instant::now() + WAIT;
        let mut connection = loop {
            match TcpStream::connect((port.address, port.port)).await {
                Ok(connection) => break connection,
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => sleep(Duration::from_millis(20)).await,
            }
        };
        let mut received = String::new();
        connection.read_to_string(&mut received).await?;

        Ok(received)
    }

    async fn refused(port: PublishedPort) -> bool {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if TcpStream::connect((port.address, port.port)).await.is_err() {
                return true;
            }
            sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn relays_published_ports_until_they_are_unpublished() {
        let (host, guest) = duplex(64 * 1024);
        let (opener, host_driver) = contr_mux::session(host, Role::Host, |_, _| async {});
        tokio::spawn(host_driver);
        let (_guest_opener, guest_driver) = contr_mux::session(guest, Role::Guest, report_port);
        tokio::spawn(guest_driver);

        let port = free_port().await;
        let (published, ports) = watch::channel(BTreeSet::from([port]));
        let forwarder = tokio::spawn(forward(ports, opener));

        assert_eq!(read_from(port).await.unwrap(), format!("guest port {}", port.port));

        published.send_replace(BTreeSet::new());
        assert!(refused(port).await);

        forwarder.abort();
    }
}
