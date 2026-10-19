//! In-process Docker Engine API server over a Unix socket or a named pipe, serving [`Fixtures`].

use std::convert::Infallible;
use std::io;
use std::sync::Arc;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task::{JoinHandle, JoinSet};

use crate::Fixtures;
use crate::routes::{Engine, route};

/// Fake Docker engine answering the endpoints the daemon's lists, watches and lifecycle calls use; logs, stats,
/// exec and pulls answer 404. Dropping it stops the server.
pub struct FakeEngine {
    #[cfg(unix)]
    socket: std::path::PathBuf,
    #[cfg(unix)]
    _dir: tempfile::TempDir,
    #[cfg(windows)]
    pipe: String,
    server: JoinHandle<()>,
}

impl FakeEngine {
    /// Serves `fixtures` on a socket in a new temporary directory.
    #[cfg(unix)]
    pub fn start(fixtures: Fixtures) -> io::Result<Self> {
        let dir = tempfile::Builder::new().prefix("contr-fake-").tempdir()?;
        let socket = dir.path().join("engine.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let engine = Arc::new(Engine::new(fixtures));

        let server = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        connections.spawn(serve_connection(stream, Arc::clone(&engine)));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "fake engine stopped accepting connections");
                        return;
                    }
                }
                while connections.try_join_next().is_some() {}
            }
        });

        Ok(Self {
            socket,
            _dir: dir,
            server,
        })
    }

    /// Serves `fixtures` on a new named pipe unique to this process.
    #[cfg(windows)]
    pub fn start(fixtures: Fixtures) -> io::Result<Self> {
        use std::sync::atomic::{AtomicU32, Ordering};

        use tokio::net::windows::named_pipe::ServerOptions;

        static NEXT: AtomicU32 = AtomicU32::new(0);

        let pipe = format!(
            r"\\.\pipe\contr-fake-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let mut instance = ServerOptions::new().first_pipe_instance(true).create(&pipe)?;
        let engine = Arc::new(Engine::new(fixtures));
        let name = pipe.clone();

        let server = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                if let Err(error) = instance.connect().await {
                    tracing::warn!(%error, "fake engine stopped accepting connections");
                    return;
                }
                let next = match ServerOptions::new().create(&name) {
                    Ok(next) => next,
                    Err(error) => {
                        tracing::warn!(%error, "fake engine cannot create a pipe instance");
                        return;
                    }
                };
                let connected = std::mem::replace(&mut instance, next);
                connections.spawn(serve_connection(connected, Arc::clone(&engine)));
                while connections.try_join_next().is_some() {}
            }
        });

        Ok(Self { pipe, server })
    }

    /// Socket the engine API listens on.
    #[cfg(unix)]
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket
    }

    /// Pipe the engine API listens on.
    #[cfg(windows)]
    pub fn pipe_name(&self) -> &str {
        &self.pipe
    }
}

impl Drop for FakeEngine {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn serve_connection<S>(stream: S, engine: Arc<Engine>)
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let service = service_fn(move |request| {
        let engine = Arc::clone(&engine);
        async move { Ok::<_, Infallible>(route(engine, request).await) }
    });

    if let Err(error) = http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await
    {
        tracing::debug!(%error, "fake engine connection ended");
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    use super::*;

    async fn get(engine: &FakeEngine, path: &str) -> String {
        let mut stream = UnixStream::connect(engine.socket_path()).await.unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn answers_ping_and_lists() {
        let engine = FakeEngine::start(Fixtures::basic()).unwrap();

        assert!(get(&engine, "/_ping").await.ends_with("OK"));
        let running = get(&engine, "/v1.51/containers/json").await;
        let all = get(&engine, "/v1.51/containers/json?all=1").await;

        assert_eq!(running.matches("\"Id\"").count(), 2);
        assert_eq!(all.matches("\"Id\"").count(), 3);
    }
}
