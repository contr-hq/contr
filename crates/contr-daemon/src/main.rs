//! `contrd`, the Contr daemon.

use std::io::IsTerminal;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use contr_core::product;
use contr_daemon::{BackendChoice, Config};
use contr_diagnostics::LogRing;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

/// Bytes of recent log output the daemon keeps for diagnostics bundles.
const LOG_RING_BYTES: usize = 2 * 1024 * 1024;

#[derive(Parser)]
#[command(name = product::DAEMON_BINARY, version = product::VERSION, about)]
struct Args {
    /// Engine backend.
    #[arg(long, value_enum, default_value_t = Backend::default())]
    backend: Backend,
    #[arg(
        long,
        value_name = "ENDPOINT",
        help = "Endpoint to serve instead of the per-user default, as unix://PATH or npipe://PATH"
    )]
    listen: Option<String>,
    /// Docker Engine API socket for the host backend; Contr then neither starts nor stops that engine.
    #[arg(long)]
    engine_socket: Option<PathBuf>,
    /// Fixture file for the fake backend.
    #[arg(long)]
    fixtures: Option<PathBuf>,
    /// Directory holding the WSL guest images; defaults to `guest` next to this executable.
    #[arg(long)]
    guest_dir: Option<PathBuf>,
    /// Leave the Docker CLI context alone.
    #[arg(long)]
    no_docker_context: bool,
    /// Do not serve the engine on the Docker CLI's default pipe (Windows).
    #[arg(long)]
    no_default_pipe: bool,
    /// Directory holding the Docker CLI tools to install for the user; defaults to `cli` next to this executable.
    #[arg(long)]
    cli_dir: Option<PathBuf>,
    /// The user's settings file; defaults to settings.json in the per-user configuration directory.
    #[arg(long)]
    settings: Option<PathBuf>,
    /// File holding the administrator's settings instead of the system's policy location.
    #[arg(long)]
    admin_settings: Option<PathBuf>,
    /// Compose CLI plugin to run instead of the bundled one or the one the Docker CLI would run.
    #[arg(long)]
    compose: Option<PathBuf>,
    /// Kubeconfig file that receives the cluster's context instead of the one kubectl writes to.
    #[arg(long)]
    kubeconfig: Option<PathBuf>,
    /// GitHub releases endpoint to check for updates instead of Contr's own, such as an internal mirror.
    #[arg(long, value_name = "URL")]
    update_feed: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Backend {
    Host,
    #[cfg(windows)]
    Wsl,
    #[cfg(feature = "fake")]
    Fake,
}

impl Default for Backend {
    #[cfg(windows)]
    fn default() -> Self {
        Self::Wsl
    }

    #[cfg(not(windows))]
    fn default() -> Self {
        Self::Host
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let log = LogRing::new(LOG_RING_BYTES);
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_env(product::LOG_ENV).unwrap_or_else(|_| EnvFilter::new("info")))
        .with(
            fmt::layer()
                .with_ansi(std::io::stderr().is_terminal())
                .with_writer(std::io::stderr),
        )
        .with(fmt::layer().with_ansi(false).with_writer(log.clone()))
        .init();

    let config = Config {
        backend: match args.backend {
            Backend::Host => BackendChoice::Host,
            #[cfg(windows)]
            Backend::Wsl => BackendChoice::Wsl,
            #[cfg(feature = "fake")]
            Backend::Fake => BackendChoice::Fake,
        },
        listen: args.listen.as_deref().map(str::parse).transpose()?,
        engine_socket: args.engine_socket,
        fixtures: args.fixtures,
        guest_dir: args.guest_dir,
        default_pipe: !args.no_default_pipe,
        settings_file: args.settings,
        admin_settings: args.admin_settings,
        cli_dir: args.cli_dir,
        docker_context: !args.no_docker_context,
        compose: args.compose,
        kubeconfig: args.kubeconfig,
        log: Some(log),
        update_feed: args.update_feed,
    };

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(contr_daemon::run(config, shutdown_signal()))?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
