//! `contr`, the command-line interface to the Contr daemon.

mod cli_tools;
mod doctor;
mod exec;
mod format;
mod settings;
mod wsl;

use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};
use contr_core::product;
use contr_sdk::api::containers::v1::ContainerState;
use contr_sdk::api::daemon::v1::BackendKind;
use contr_sdk::api::engine::v1::{
    EngineState, ResetRequest, ResetScope, StartRequest, StopRequest, reset_response, start_response, stop_response,
};
use contr_sdk::{Client, ClientIdentity, Error};

#[derive(Parser)]
#[command(name = product::CLI_BINARY, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the daemon and engine status.
    Status,
    /// Start the engine, starting the daemon first when needed.
    Start,
    /// Stop the engine.
    Stop,
    /// Stop the engine and replace its runtime, keeping images, containers and volumes.
    Reset {
        /// Delete every image, container, volume and the build cache instead.
        #[arg(long)]
        factory: bool,
    },
    /// List containers.
    Ps {
        /// Show stopped containers too.
        #[arg(short, long)]
        all: bool,
    },
    /// Run a command in a running container.
    Exec(exec::ExecArgs),
    /// Show or install the Docker CLI tools the daemon bundles.
    #[command(subcommand)]
    Cli(cli_tools::CliCommand),
    /// Show or change your settings.
    Settings {
        #[command(subcommand)]
        command: Option<settings::SettingsCommand>,
    },
    /// Check what keeps the engine or its features from working.
    Doctor,
    /// Run a fix that `doctor` offers.
    Fix {
        /// Identifier `doctor` prints, such as install-wsl.
        action_id: String,
    },
    /// Show or change which of your WSL distributions reach the engine.
    #[command(subcommand)]
    Wsl(wsl::WslCommand),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => return fail(&error.into()),
    };

    match runtime.block_on(run(cli.command)) {
        // The exec's stdin reader may still block on the terminal; exiting skips waiting for it.
        Ok(code) => std::process::exit(code),
        Err(error) => fail(&error),
    }
}

async fn run(command: Command) -> anyhow::Result<i32> {
    let identity = ClientIdentity {
        name: product::CLI_BINARY.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };

    match command {
        Command::Status => status(&identity).await,
        Command::Start => start(&Client::connect_or_start(&identity).await?).await,
        Command::Stop => stop(&Client::connect_default(&identity).await?).await,
        Command::Reset { factory } => reset(&Client::connect_or_start(&identity).await?, factory).await,
        Command::Ps { all } => ps(&Client::connect_default(&identity).await?, all).await,
        Command::Exec(args) => exec::run(&Client::connect_default(&identity).await?, args).await,
        Command::Cli(command) => cli_tools::run(&Client::connect_or_start(&identity).await?, command).await,
        Command::Wsl(command) => wsl::run(&Client::connect_or_start(&identity).await?, command).await,
        Command::Settings { command } => settings::run(&Client::connect_or_start(&identity).await?, command).await,
        Command::Doctor => doctor::doctor(&Client::connect_or_start(&identity).await?).await,
        Command::Fix { action_id } => doctor::fix(&Client::connect_or_start(&identity).await?, action_id).await,
    }
}

fn fail(error: &anyhow::Error) -> ExitCode {
    eprintln!("{}: {error:#}", product::CLI_BINARY);
    match error.downcast_ref::<Error>() {
        Some(Error::EngineNotRunning(_)) => {
            eprintln!("Start the engine with `{} start`.", product::CLI_BINARY);
        }
        Some(Error::Connect { .. }) => {
            eprintln!("Start the daemon with `{} start`.", product::CLI_BINARY);
        }
        _ => {}
    }
    ExitCode::FAILURE
}

async fn status(identity: &ClientIdentity) -> anyhow::Result<i32> {
    let client = match Client::connect_default(identity).await {
        Ok(client) => client,
        Err(Error::Connect { endpoint, .. }) => {
            println!("Daemon:    not running ({endpoint})");
            return Ok(1);
        }
        Err(error) => return Err(error.into()),
    };
    let info = client.info().await?;
    let state = client.engine_state().await?;

    println!(
        "Daemon:    running ({} {}, API revision {}, pid {})",
        product::DAEMON_BINARY,
        info.daemon_version,
        info.api_revision,
        info.pid
    );
    println!("Backend:   {}", backend_name(info.backend_kind()));
    match state.state() {
        EngineState::Failed => println!("Engine:    Failed ({})", state.failure_reason().as_str_name()),
        other => println!("Engine:    {}", state_name(other)),
    }
    if let Some(engine) = info.engine {
        let mode = if engine.rootless { "rootless" } else { "rootful" };
        println!(
            "Version:   {} (API {}, {}/{}, {mode})",
            engine.version, engine.api_version, engine.os, engine.arch
        );
        println!("Endpoint:  {}", engine.endpoint);
        if !engine.docker_context.is_empty() {
            println!("Context:   {}", engine.docker_context);
        }
    }

    Ok(i32::from(state.state() != EngineState::Running))
}

async fn start(client: &Client) -> anyhow::Result<i32> {
    let mut events = client
        .engine()
        .start(StartRequest {})
        .await
        .map_err(Error::from)?
        .into_inner();
    while let Some(event) = events.message().await.map_err(Error::from)? {
        match event.event {
            Some(start_response::Event::Progress(progress)) => eprintln!("{}", progress.message),
            Some(start_response::Event::State(state)) => {
                println!(
                    "Engine:    {}",
                    state_name(EngineState::try_from(state).unwrap_or_default())
                );
            }
            None => {}
        }
    }
    Ok(0)
}

async fn stop(client: &Client) -> anyhow::Result<i32> {
    let mut events = client
        .engine()
        .stop(StopRequest {})
        .await
        .map_err(Error::from)?
        .into_inner();
    while let Some(event) = events.message().await.map_err(Error::from)? {
        match event.event {
            Some(stop_response::Event::Progress(progress)) => eprintln!("{}", progress.message),
            Some(stop_response::Event::State(state)) => {
                println!(
                    "Engine:    {}",
                    state_name(EngineState::try_from(state).unwrap_or_default())
                );
            }
            None => {}
        }
    }
    Ok(0)
}

async fn reset(client: &Client, factory: bool) -> anyhow::Result<i32> {
    let scope = if factory {
        ResetScope::Factory
    } else {
        ResetScope::Runtime
    };
    let mut events = client
        .engine()
        .reset(ResetRequest { scope: scope.into() })
        .await
        .map_err(Error::from)?
        .into_inner();
    while let Some(event) = events.message().await.map_err(Error::from)? {
        match event.event {
            Some(reset_response::Event::Progress(progress)) => eprintln!("{}", progress.message),
            Some(reset_response::Event::State(state)) => {
                println!(
                    "Engine:    {}",
                    state_name(EngineState::try_from(state).unwrap_or_default())
                );
            }
            None => {}
        }
    }
    Ok(0)
}

async fn ps(client: &Client, all: bool) -> anyhow::Result<i32> {
    let mut containers = client.list_containers().await.context("cannot list containers")?;
    containers.retain(|container| all || container.state() == ContainerState::Running);
    containers.sort_by_key(|container| std::cmp::Reverse(container.created_at.map(|created| created.seconds)));

    let rows: Vec<Vec<String>> = containers
        .iter()
        .map(|container| {
            vec![
                format::short_id(&container.id).to_owned(),
                container.image.clone(),
                format::command(&container.command),
                format::ago(container.created_at.as_ref()),
                container.status.clone(),
                format::ports(&container.ports),
                container.names.join(","),
            ]
        })
        .collect();

    print!(
        "{}",
        format::table(
            &[
                "CONTAINER ID",
                "IMAGE",
                "COMMAND",
                "CREATED",
                "STATUS",
                "PORTS",
                "NAMES"
            ],
            &rows
        )
    );
    Ok(0)
}

fn backend_name(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Host => "host engine",
        BackendKind::Wsl => "WSL",
        BackendKind::Qemu => "virtual machine",
        BackendKind::Fake => "fake",
        BackendKind::Unspecified => "unknown",
    }
}

fn state_name(state: EngineState) -> &'static str {
    match state {
        EngineState::Unspecified => "Unknown",
        EngineState::Uninitialized => "Not set up",
        EngineState::Provisioning => "Setting up",
        EngineState::Stopped => "Stopped",
        EngineState::Starting => "Starting",
        EngineState::Running => "Running",
        EngineState::Stopping => "Stopping",
        EngineState::Updating => "Updating",
        EngineState::Resetting => "Resetting",
        EngineState::Failed => "Failed",
    }
}
