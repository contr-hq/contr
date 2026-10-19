use hyper_util::rt::TokioIo;
use tonic::transport::{Channel, Uri};
use tower::service_fn;

use crate::{Endpoint, TransportError};

/// Opens a gRPC channel to the daemon at `endpoint`.
pub async fn connect(endpoint: &Endpoint) -> Result<Channel, TransportError> {
    let target = endpoint.clone();
    let connector = service_fn(move |_: Uri| {
        let target = target.clone();
        async move { open(&target).await.map(TokioIo::new) }
    });

    // HTTP/2 needs an authority; the connector ignores it.
    let channel = tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(connector)
        .await?;

    Ok(channel)
}

#[cfg(unix)]
async fn open(endpoint: &Endpoint) -> std::io::Result<tokio::net::UnixStream> {
    match endpoint {
        Endpoint::Unix(path) => tokio::net::UnixStream::connect(path).await,
        Endpoint::NamedPipe(_) => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "named pipes exist only on Windows",
        )),
    }
}

#[cfg(windows)]
async fn open(endpoint: &Endpoint) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    match endpoint {
        Endpoint::NamedPipe(name) => crate::pipe::open_client(name).await,
        Endpoint::Unix(_) => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the daemon serves named pipes on Windows",
        )),
    }
}
