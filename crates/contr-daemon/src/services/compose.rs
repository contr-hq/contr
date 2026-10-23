use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use contr_api::common::v1::Progress;
use contr_api::compose::v1 as proto;
use contr_api::compose::v1::compose_service_server::ComposeService;
use contr_api::compose::v1::run_action_response;
use contr_api::containers::v1::{Container, StreamLogsRequest};
use contr_api::operations::v1::follow_operation_response::Event as OperationEvent;
use contr_api::operations::v1::{OperationRequest, operation_request};
use contr_compose::{Added, Compose, ComposeError, Declared, Output, Target};
use contr_engine::Change;
use contr_settings::ComposeProject;
use futures_util::{StreamExt, stream};
use tokio::sync::Mutex;
use tokio_stream::wrappers::WatchStream;
use tonic::{Request, Response, Status};

use super::{ResponseStream, Shared, follow_started};
use crate::operations::{Admission, Started};
use crate::status;

/// Compose projects, and Compose run against the engine.
#[derive(Clone)]
pub(crate) struct ComposeApi {
    shared: Arc<Shared>,
    support: Arc<ComposeSupport>,
}

impl ComposeApi {
    pub(crate) fn new(shared: Arc<Shared>, program: Option<PathBuf>) -> Self {
        Self {
            shared,
            support: Arc::new(ComposeSupport {
                program,
                read: Mutex::default(),
            }),
        }
    }

    /// Compose against the running engine; fails with `FEATURE_UNAVAILABLE` where none is installed.
    async fn compose(&self) -> Result<Compose, Status> {
        let engine = self.shared.slot.get().await?;
        self.support
            .compose(&self.shared, engine.engine.endpoint())
            .ok_or_else(|| status::feature_unavailable("Compose is not installed"))
    }

    async fn project(&self, name: &str) -> Result<proto::Project, Status> {
        let engine = self.shared.slot.get().await?;
        let compose = self.support.compose(&self.shared, engine.engine.endpoint());
        let added = self.shared.settings.snapshot().compose_projects;

        self.support
            .projects(&engine.cache.containers(), &added, compose.as_ref())
            .await
            .into_iter()
            .find(|project| project.name == name)
            .ok_or_else(|| Status::not_found(format!("no Compose project is named {name}")))
    }

    /// Starts `request.action` on its project; fails with `FAILED_PRECONDITION` while another action runs on it.
    pub(super) async fn start_action(&self, request: proto::RunActionRequest) -> Result<Started, Status> {
        let project = self.project(&request.project).await?;
        let compose = self.compose().await?;
        let target = Target {
            project: project.name.clone(),
            working_dir: project.working_dir,
            config_files: project.config_files,
        };
        let invocation = compose
            .prepare(&target, request.action(), &request.services, request.remove_volumes)
            .map_err(|error| compose_status(&error))?;
        let admission = Admission::Exclusive {
            key: format!("compose {}", project.name),
            conflict: format!("a Compose action already runs on project {}", project.name),
        };
        let operation = OperationRequest {
            kind: Some(operation_request::Kind::ComposeAction(request)),
        };

        self.shared
            .operations
            .start(operation, true, admission, move |sink| async move {
                let output = invocation.run().map_err(|error| compose_status(&error))?;
                let mut output = std::pin::pin!(output);
                let mut exit = None;
                while let Some(output) = output.next().await {
                    let event = match output
                        .map_err(|error| Status::internal(format!("cannot read Compose's output: {error}")))?
                    {
                        Output::Line(line) => {
                            sink.progress(Progress {
                                message: line.clone(),
                                fraction: None,
                            });
                            run_action_response::Event::Output(line)
                        }
                        Output::Exit(code) => {
                            exit = Some(code);
                            run_action_response::Event::ExitCode(code)
                        }
                    };
                    sink.emit(OperationEvent::ComposeAction(proto::RunActionResponse {
                        event: Some(event),
                    }));
                }
                match exit {
                    Some(0) => Ok(()),
                    Some(code) => Err(Status::unknown(format!("Compose exited with code {code}"))),
                    None => Err(Status::internal("Compose ended without an exit code")),
                }
            })
    }
}

#[tonic::async_trait]
impl ComposeService for ComposeApi {
    type WatchProjectsStream = ResponseStream<proto::WatchProjectsResponse>;
    type RunActionStream = ResponseStream<proto::RunActionResponse>;
    type StreamLogsStream = ResponseStream<proto::StreamLogsResponse>;

    async fn list_projects(
        &self,
        _: Request<proto::ListProjectsRequest>,
    ) -> Result<Response<proto::ListProjectsResponse>, Status> {
        let engine = self.shared.slot.get().await?;
        let compose = self.support.compose(&self.shared, engine.engine.endpoint());
        let added = self.shared.settings.snapshot().compose_projects;
        let projects = self
            .support
            .projects(&engine.cache.containers(), &added, compose.as_ref())
            .await;

        Ok(Response::new(proto::ListProjectsResponse { projects }))
    }

    async fn watch_projects(
        &self,
        _: Request<proto::WatchProjectsRequest>,
    ) -> Result<Response<Self::WatchProjectsStream>, Status> {
        let engine = self.shared.slot.get().await?;
        let compose = self.support.compose(&self.shared, engine.engine.endpoint());
        let mut changes = engine.cache.watch_containers();
        let mut settings = WatchStream::new(self.shared.settings.subscribe());
        drop(engine);
        let support = Arc::clone(&self.support);

        let projects = async_stream::stream! {
            let mut containers: HashMap<String, Container> = HashMap::new();
            let mut listed = false;
            let mut added: Vec<ComposeProject> = Vec::new();
            let mut sent = None;
            loop {
                tokio::select! {
                    change = changes.next() => match change {
                        Some(Change::Snapshot(all)) => {
                            containers = all.into_iter().map(|container| (container.id.clone(), container)).collect();
                            listed = true;
                        }
                        Some(Change::Upsert(container)) => {
                            containers.insert(container.id.clone(), container);
                        }
                        Some(Change::Remove(id)) => {
                            containers.remove(&id);
                        }
                        None => break,
                    },
                    snapshot = settings.next() => match snapshot {
                        Some(snapshot) => added = snapshot.compose_projects,
                        None => break,
                    },
                }
                if !listed {
                    continue;
                }
                let current: Vec<Container> = containers.values().cloned().collect();
                let projects = support.projects(&current, &added, compose.as_ref()).await;
                if sent.as_ref() != Some(&projects) {
                    sent = Some(projects.clone());
                    yield Ok(proto::WatchProjectsResponse { projects });
                }
            }
            yield Err(status::engine_not_running());
        };

        Ok(Response::new(self.shared.until_stopping(projects.boxed())))
    }

    async fn add_project(
        &self,
        request: Request<proto::AddProjectRequest>,
    ) -> Result<Response<proto::AddProjectResponse>, Status> {
        let config_files = request.into_inner().config_files;
        if config_files.is_empty() {
            return Err(Status::invalid_argument("name at least one Compose file"));
        }
        if let Some(relative) = config_files.iter().find(|file| !Path::new(file).is_absolute()) {
            return Err(Status::invalid_argument(format!("{relative} is not an absolute path")));
        }
        let compose = self.compose().await?;
        let declared = compose.config(&config_files).await.map_err(Status::invalid_argument)?;
        self.support.remember(&config_files, Ok(declared.clone())).await;
        let name = contr_compose::added_name(&config_files, &Ok(declared));

        self.shared
            .settings
            .update(None, self.shared.backend.as_ref(), |stored| {
                if !stored
                    .compose_projects
                    .iter()
                    .any(|project| project.config_files == config_files)
                {
                    stored.compose_projects.push(ComposeProject { config_files });
                }
                Ok(())
            })
            .await?;

        Ok(Response::new(proto::AddProjectResponse {
            project: Some(self.project(&name).await?),
        }))
    }

    async fn remove_project(
        &self,
        request: Request<proto::RemoveProjectRequest>,
    ) -> Result<Response<proto::RemoveProjectResponse>, Status> {
        let name = request.into_inner().name;
        let compose = self.compose().await.ok();
        let added = self.shared.settings.snapshot().compose_projects;
        let mut remove = Vec::new();
        for project in &added {
            let declared = self.support.declared(compose.as_ref(), &project.config_files).await;
            let declared = declared.unwrap_or_else(|| Err(String::new()));
            if contr_compose::added_name(&project.config_files, &declared) == name {
                remove.push(project.config_files.clone());
            }
        }
        if remove.is_empty() {
            return Err(Status::not_found(format!("no added Compose project is named {name}")));
        }

        self.shared
            .settings
            .update(None, self.shared.backend.as_ref(), |stored| {
                stored
                    .compose_projects
                    .retain(|project| !remove.contains(&project.config_files));
                Ok(())
            })
            .await?;
        Ok(Response::new(proto::RemoveProjectResponse {}))
    }

    async fn run_action(
        &self,
        request: Request<proto::RunActionRequest>,
    ) -> Result<Response<Self::RunActionStream>, Status> {
        let started = self.start_action(request.into_inner()).await?;
        let events = follow_started(
            &self.shared,
            &started,
            true,
            |event| match event {
                OperationEvent::ComposeAction(event) => Some(event),
                _ => None,
            },
            |event| {
                matches!(
                    event,
                    OperationEvent::ComposeAction(proto::RunActionResponse {
                        event: Some(run_action_response::Event::ExitCode(_))
                    })
                )
            },
        )?;

        Ok(Response::new(events))
    }

    async fn stream_logs(
        &self,
        request: Request<proto::StreamLogsRequest>,
    ) -> Result<Response<Self::StreamLogsStream>, Status> {
        let request = request.into_inner();
        let project = self.project(&request.project).await?;
        if let Some(unknown) = request
            .services
            .iter()
            .find(|name| !project.services.iter().any(|service| service.name == **name))
        {
            return Err(Status::invalid_argument(format!(
                "{} has no service named {unknown}",
                project.name
            )));
        }
        let engine = self.shared.slot.get().await?;

        let streams = project
            .services
            .into_iter()
            .filter(|service| request.services.is_empty() || request.services.contains(&service.name))
            .flat_map(|service| {
                let name = service.name;
                service
                    .containers
                    .into_iter()
                    .map(move |container| (name.clone(), container.id))
            })
            .map(|(service, container_id)| {
                let logs = engine.engine.logs(&StreamLogsRequest {
                    id: container_id.clone(),
                    follow: request.follow,
                    tail_lines: request.tail_lines,
                    since: None,
                    timestamps: request.timestamps,
                });
                logs.map(move |chunk| {
                    chunk
                        .map_err(status::from_engine)
                        .map(|chunk| proto::StreamLogsResponse {
                            service: service.clone(),
                            container_id: container_id.clone(),
                            stream: chunk.stream,
                            data: chunk.data,
                            timestamp: chunk.timestamp,
                        })
                })
                .boxed()
            })
            .collect::<Vec<_>>();

        Ok(Response::new(
            self.shared.until_stopping(stream::select_all(streams).boxed()),
        ))
    }
}

/// Where Compose is, and what it last read from each set of Compose files.
struct ComposeSupport {
    /// Compose given on the command line; otherwise it is looked up for every call.
    program: Option<PathBuf>,
    read: Mutex<HashMap<Vec<String>, Read>>,
}

/// What Compose read from a set of Compose files while they had these modification times.
struct Read {
    modified: Vec<Option<SystemTime>>,
    declared: Result<Declared, String>,
}

impl ComposeSupport {
    fn compose(&self, shared: &Shared, docker_host: &str) -> Option<Compose> {
        let bundle = shared.cli.as_ref().map(|cli| cli.bundle.join("cli-plugins"));
        let program = self.program.clone().or_else(|| Compose::find(bundle.as_deref()))?;

        Some(Compose::new(program, docker_host.to_owned()))
    }

    /// Projects of `containers` and the `added` ones.
    async fn projects(
        &self,
        containers: &[Container],
        added: &[ComposeProject],
        compose: Option<&Compose>,
    ) -> Vec<proto::Project> {
        let mut sets: Vec<Vec<String>> = containers
            .iter()
            .filter(|container| container.labels.contains_key(contr_compose::labels::PROJECT))
            .map(contr_compose::config_files)
            .chain(added.iter().map(|project| project.config_files.clone()))
            .filter(|files| !files.is_empty())
            .collect();
        sets.sort();
        sets.dedup();
        let mut reads = HashMap::new();
        for files in sets {
            if let Some(declared) = self.declared(compose, &files).await {
                reads.insert(files, declared);
            }
        }

        let unread = Err("Compose is not installed, so the project's files cannot be read".to_owned());
        let added: Vec<Added<'_>> = added
            .iter()
            .map(|project| Added {
                config_files: &project.config_files,
                declared: reads.get(&project.config_files).unwrap_or(&unread),
            })
            .collect();
        contr_compose::group(containers, &added, |files| reads.get(files).cloned())
    }

    /// What `files` declare, read again only when one of them changed; `None` without Compose.
    async fn declared(&self, compose: Option<&Compose>, files: &[String]) -> Option<Result<Declared, String>> {
        let compose = compose?;
        let modified = modification_times(files);
        let mut read = self.read.lock().await;
        if let Some(earlier) = read.get(files).filter(|earlier| earlier.modified == modified) {
            return Some(earlier.declared.clone());
        }
        let declared = compose.config(files).await;
        read.insert(
            files.to_vec(),
            Read {
                modified,
                declared: declared.clone(),
            },
        );

        Some(declared)
    }

    /// Records what Compose just read from `files`.
    async fn remember(&self, files: &[String], declared: Result<Declared, String>) {
        self.read.lock().await.insert(
            files.to_vec(),
            Read {
                modified: modification_times(files),
                declared,
            },
        );
    }
}

fn modification_times(files: &[String]) -> Vec<Option<SystemTime>> {
    files
        .iter()
        .map(|file| std::fs::metadata(file).and_then(|metadata| metadata.modified()).ok())
        .collect()
}

fn compose_status(error: &ComposeError) -> Status {
    match error {
        ComposeError::NoConfigFiles(_) => Status::failed_precondition(error.to_string()),
        ComposeError::UnknownAction(_) => Status::invalid_argument(error.to_string()),
        ComposeError::Spawn(source) => Status::internal(format!("{error}: {source}")),
    }
}
