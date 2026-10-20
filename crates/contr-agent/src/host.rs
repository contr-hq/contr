use std::net::{Ipv4Addr, SocketAddrV4};
use std::process::Stdio;

use anyhow::{Context, bail, ensure};
use contr_core::guest;
use contr_mux::Opener;
use nix::sys::socket::{getsockopt, sockopt::OriginalDst};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;

/// Port that redirected `host.docker.internal` connections arrive on.
const LISTEN_PORT: u16 = 47_321;

/// Redirects TCP aimed at [`guest::HOST_ADDRESS`] to the agent and relays each connection to the host's loopback
/// interface through `opener`, so it originates on the host, where no firewall rule applies.
pub(crate) async fn forward(opener: Opener) -> anyhow::Result<()> {
    install_redirect().await?;
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, LISTEN_PORT))
        .await
        .with_context(|| format!("cannot listen on port {LISTEN_PORT}"))?;
    tracing::info!(address = %guest::HOST_ADDRESS, "forwarding host connections");

    loop {
        let (connection, _) = listener.accept().await?;
        let opener = opener.clone();
        tokio::spawn(async move {
            if let Err(error) = relay(connection, &opener).await {
                tracing::debug!(%error, "host connection ended");
            }
        });
    }
}

async fn relay(mut connection: TcpStream, opener: &Opener) -> anyhow::Result<()> {
    let destination = original_destination(&connection)?;
    if *destination.ip() != guest::HOST_ADDRESS {
        bail!("{destination} was not reached through the redirect");
    }
    let mut stream = opener.open_host_tcp(destination.port()).await?;
    tokio::io::copy_bidirectional(&mut connection, &mut stream).await?;

    Ok(())
}

/// Destination the connection had before the redirect rewrote it.
fn original_destination(connection: &TcpStream) -> nix::Result<SocketAddrV4> {
    let address = getsockopt(connection, OriginalDst)?;

    Ok(SocketAddrV4::new(
        Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr)),
        u16::from_be(address.sin_port),
    ))
}

/// Replaces the `contr` nftables table. Its priority runs it before the engine's own NAT rules, so a port published
/// by a container never captures `host.docker.internal` traffic.
async fn install_redirect() -> anyhow::Result<()> {
    let address = guest::HOST_ADDRESS;
    let rules = format!(
        "table ip contr
delete table ip contr
table ip contr {{
    chain prerouting {{
        type nat hook prerouting priority -110; policy accept;
        ip daddr {address} meta l4proto tcp redirect to :{LISTEN_PORT}
    }}
    chain output {{
        type nat hook output priority -110; policy accept;
        ip daddr {address} meta l4proto tcp redirect to :{LISTEN_PORT}
    }}
}}
"
    );

    let mut nft = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .context("cannot run nft")?;
    let mut stdin = nft.stdin.take().context("nft has no standard input")?;
    stdin.write_all(rules.as_bytes()).await?;
    drop(stdin);
    let status = nft.wait().await?;
    ensure!(status.success(), "nft could not install the host redirect ({status})");

    Ok(())
}
