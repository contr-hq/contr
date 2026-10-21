use std::io::Write;
use std::path::PathBuf;

use clap::Subcommand;
use contr_sdk::api::compose::v1::{
    Action, AddProjectRequest, ListProjectsRequest, Project, RemoveProjectRequest, RunActionRequest, StreamLogsRequest,
    run_action_response,
};
use contr_sdk::api::containers::v1::ContainerState;
use contr_sdk::{Client, Error};

use crate::format;

#[derive(Subcommand)]
pub(crate) enum ComposeCommand {
    /// List Compose projects.
    Ls,
    /// Keep a project listed by its Compose files, even while it has no containers.
    Add {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Stop listing a project you added; its containers stay.
    Remove { project: String },
    /// Create and start a project's services in the background.
    Up { project: String, services: Vec<String> },
    /// Pull a project's images.
    Pull { project: String, services: Vec<String> },
    /// Start a project's stopped services.
    Start { project: String, services: Vec<String> },
    /// Stop a project's services.
    Stop { project: String, services: Vec<String> },
    /// Restart a project's services.
    Restart { project: String, services: Vec<String> },
    /// Stop and remove a project's containers and networks.
    Down {
        project: String,
        /// Also remove the project's named volumes.
        #[arg(long)]
        volumes: bool,
    },
    /// Show a project's logs, each line prefixed with its service.
    Logs {
        project: String,
        services: Vec<String>,
        /// Keep following new output.
        #[arg(short, long)]
        follow: bool,
        /// Lines to show from the end of each container's log.
        #[arg(long)]
        tail: Option<u32>,
    },
}

pub(crate) async fn run(client: &Client, command: ComposeCommand) -> anyhow::Result<i32> {
    let mut compose = client.compose();
    let (project, action, services, remove_volumes) = match command {
        ComposeCommand::Ls => {
            let projects = compose
                .list_projects(ListProjectsRequest {})
                .await
                .map_err(Error::from)?
                .into_inner()
                .projects;
            print_projects(&projects);
            return Ok(0);
        }
        ComposeCommand::Add { files } => {
            let config_files = files
                .iter()
                .map(|file| std::path::absolute(file).map(|file| file.display().to_string()))
                .collect::<Result<_, _>>()?;
            let project = compose
                .add_project(AddProjectRequest { config_files })
                .await
                .map_err(Error::from)?
                .into_inner()
                .project
                .unwrap_or_default();
            println!("Added {}.", project.name);
            return Ok(0);
        }
        ComposeCommand::Remove { project } => {
            compose
                .remove_project(RemoveProjectRequest { name: project })
                .await
                .map_err(Error::from)?;
            return Ok(0);
        }
        ComposeCommand::Logs {
            project,
            services,
            follow,
            tail,
        } => return logs(client, project, services, follow, tail).await,
        ComposeCommand::Up { project, services } => (project, Action::Up, services, false),
        ComposeCommand::Pull { project, services } => (project, Action::Pull, services, false),
        ComposeCommand::Start { project, services } => (project, Action::Start, services, false),
        ComposeCommand::Stop { project, services } => (project, Action::Stop, services, false),
        ComposeCommand::Restart { project, services } => (project, Action::Restart, services, false),
        ComposeCommand::Down { project, volumes } => (project, Action::Down, Vec::new(), volumes),
    };

    let mut events = compose
        .run_action(RunActionRequest {
            project,
            action: action.into(),
            services,
            remove_volumes,
        })
        .await
        .map_err(Error::from)?
        .into_inner();
    while let Some(event) = events.message().await.map_err(Error::from)? {
        match event.event {
            Some(run_action_response::Event::Output(line)) => println!("{line}"),
            Some(run_action_response::Event::ExitCode(code)) => return Ok(code),
            None => {}
        }
    }

    Ok(1)
}

async fn logs(
    client: &Client,
    project: String,
    services: Vec<String>,
    follow: bool,
    tail_lines: Option<u32>,
) -> anyhow::Result<i32> {
    let mut chunks = client
        .compose()
        .stream_logs(StreamLogsRequest {
            project,
            services,
            follow,
            tail_lines,
            timestamps: false,
        })
        .await
        .map_err(Error::from)?
        .into_inner();
    let mut stdout = std::io::stdout().lock();
    while let Some(chunk) = chunks.message().await.map_err(Error::from)? {
        for line in String::from_utf8_lossy(&chunk.data).lines() {
            writeln!(stdout, "{} | {line}", chunk.service)?;
        }
    }

    Ok(0)
}

fn print_projects(projects: &[Project]) {
    let rows: Vec<Vec<String>> = projects
        .iter()
        .map(|project| {
            let containers: Vec<_> = project
                .services
                .iter()
                .flat_map(|service| &service.containers)
                .collect();
            let running = containers
                .iter()
                .filter(|container| container.state() == ContainerState::Running)
                .count();
            let status = if containers.is_empty() {
                "not created".to_owned()
            } else {
                format!("{running} of {} running", containers.len())
            };
            let location = project
                .config_files
                .first()
                .cloned()
                .unwrap_or_else(|| project.working_dir.clone());
            let note = if project.config_error.is_empty() {
                ""
            } else {
                "files unreadable"
            };
            vec![project.name.clone(), status, location, note.to_owned()]
        })
        .collect();

    print!("{}", format::table(&["PROJECT", "STATUS", "COMPOSE FILE", ""], &rows));
}
