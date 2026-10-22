use contr_mux::{Channel, MuxStream};

/// Serves a stream the guest opens: a [`Channel::HostTcp`] stream is joined to the port it names on this machine's
/// loopback interface.
pub(crate) async fn serve(channel: Channel, stream: MuxStream) {
    if channel != Channel::HostTcp {
        tracing::warn!(?channel, "the guest opened an unsupported channel");
        return;
    }
    if let Err(error) = contr_mux::join_loopback(stream).await {
        tracing::debug!(%error, "host connection ended");
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use contr_mux::Role;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn guest_connections_reach_loopback_listeners() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"hello from host").await.unwrap();
        });

        let (host, guest) = duplex(64 * 1024);
        let (_host_opener, host_driver) = contr_mux::session(host, Role::Host, serve);
        tokio::spawn(host_driver);
        let (guest_opener, guest_driver) = contr_mux::session(guest, Role::Guest, |_, _| async {});
        tokio::spawn(guest_driver);

        let mut stream = guest_opener.open_host_tcp(port).await.unwrap();
        let mut received = Vec::new();
        stream.read_to_end(&mut received).await.unwrap();

        assert_eq!(received, b"hello from host");
    }
}
