use std::path::PathBuf;
use std::sync::Arc;

use contr_api::common::v1 as common;
use contr_api::diagnostics::v1 as proto;
use contr_api::diagnostics::v1::diagnostics_service_server::DiagnosticsService;
use contr_api::diagnostics::v1::{create_bundle_response, run_remediation_response::Event};
use contr_api::operations::v1::follow_operation_response::Event as OperationEvent;
use contr_api::operations::v1::{OperationRequest, operation_request};
use contr_backend::{Backend, BackendError, Check, CheckStatus, EngineState, Progress, ProgressSink};
use contr_core::Remediation;
use contr_diagnostics::{Bundle, BundleError, Identity, Redactor};
use serde_json::{Value, json};
use tonic::{Request, Response, Status};

use super::{ResponseStream, Shared, follow_started};
use crate::operations::{Admission, Sink, Started};

const GIB: u64 = 1024 * 1024 * 1024;
const DISK_FAIL_BELOW: u64 = 2 * GIB;
const DISK_WARN_BELOW: u64 = 10 * GIB;
const BUNDLE_CHUNK: usize = 64 * 1024;
const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";
/// Check of the backends that serve the Docker CLI's default endpoint, which the `default` context reaches.
const DEFAULT_PIPE_CHECK: &str = "default-pipe";
const DEFAULT_CONTEXT: &str = "default";

pub(crate) struct DiagnosticsApi(pub(crate) Arc<Shared>);

#[tonic::async_trait]
impl DiagnosticsService for DiagnosticsApi {
    type RunRemediationStream = ResponseStream<proto::RunRemediationResponse>;
    type CreateBundleStream = ResponseStream<proto::CreateBundleResponse>;

    async fn run_checks(
        &self,
        _: Request<proto::RunChecksRequest>,
    ) -> Result<Response<proto::RunChecksResponse>, Status> {
        let checks = run_all_checks(&self.0).await;

        Ok(Response::new(proto::RunChecksResponse {
            checks: checks.into_iter().map(check).collect(),
        }))
    }

    async fn run_remediation(
        &self,
        request: Request<proto::RunRemediationRequest>,
    ) -> Result<Response<Self::RunRemediationStream>, Status> {
        let started = start_remediation(&self.0, request.into_inner())?;
        let events = follow_started(
            &self.0,
            &started,
            false,
            |event| match event {
                OperationEvent::RunRemediation(event) => Some(event),
                _ => None,
            },
            |_| false,
        )?;

        Ok(Response::new(events))
    }

    async fn create_bundle(
        &self,
        _: Request<proto::CreateBundleRequest>,
    ) -> Result<Response<Self::CreateBundleStream>, Status> {
        let started = start_bundle(&self.0)?;
        let events = follow_started(
            &self.0,
            &started,
            true,
            |event| match event {
                OperationEvent::CreateBundle(event) => Some(event),
                _ => None,
            },
            |_| false,
        )?;

        Ok(Response::new(events))
    }
}

/// Starts the remediation `request.action_id` names, or returns the one already running.
pub(super) fn start_remediation(
    shared: &Arc<Shared>,
    request: proto::RunRemediationRequest,
) -> Result<Started, Status> {
    let remediation = shared
        .backend
        .remediations()
        .iter()
        .find(|remediation| remediation.action_id == request.action_id)
        .cloned()
        .ok_or_else(|| Status::not_found(format!("this backend offers no remediation {:?}", request.action_id)))?;
    let key = format!("remediation {}", request.action_id);
    let operation = OperationRequest {
        kind: Some(operation_request::Kind::RunRemediation(request)),
    };
    let backend = Arc::clone(&shared.backend);

    shared
        .operations
        .start(operation, false, Admission::Join(key), move |sink| async move {
            match backend.remediate(&remediation, &RemediationSink(sink.clone())).await {
                Ok(outcome) => {
                    sink.emit(OperationEvent::RunRemediation(proto::RunRemediationResponse {
                        event: Some(Event::Outcome(proto::RemediationOutcome {
                            restart_required: outcome.restart_required,
                            logout_required: outcome.logout_required,
                        })),
                    }));
                    Ok(())
                }
                Err(error @ BackendError::Dismissed) => Err(Status::cancelled(format!(
                    "{} did not run: {error}",
                    remediation.action_id
                ))),
                Err(error) => Err(Status::failed_precondition(format!(
                    "{} failed: {error}",
                    remediation.action_id
                ))),
            }
        })
}

/// Starts writing a diagnostics bundle, or returns the one already being written; the zip arrives in chunks once it
/// is whole.
pub(super) fn start_bundle(shared: &Arc<Shared>) -> Result<Started, Status> {
    let redactor = Redactor::new(&Identity::current()).map_err(|error| Status::internal(error.to_string()))?;
    let operation = OperationRequest {
        kind: Some(operation_request::Kind::CreateBundle(proto::CreateBundleRequest {})),
    };
    let shared = Arc::clone(shared);

    Arc::clone(&shared.operations).start(
        operation,
        true,
        Admission::Join("bundle".to_owned()),
        move |sink| async move {
            let step = |message: &str| {
                let progress = common::Progress {
                    message: message.to_owned(),
                    fraction: None,
                };
                sink.progress(progress.clone());
                sink.emit(OperationEvent::CreateBundle(proto::CreateBundleResponse {
                    event: Some(create_bundle_response::Event::Progress(progress)),
                }));
            };
            let zip = write_bundle(&shared, redactor, step)
                .await
                .map_err(|error| Status::internal(format!("cannot write the bundle: {error}")))?;
            for chunk in zip.chunks(BUNDLE_CHUNK) {
                sink.emit(OperationEvent::CreateBundle(proto::CreateBundleResponse {
                    event: Some(create_bundle_response::Event::Chunk(chunk.to_vec())),
                }));
            }
            Ok(())
        },
    )
}

async fn run_all_checks(shared: &Shared) -> Vec<Check> {
    let mut checks = vec![prerequisites(shared.backend.as_ref()).await];
    checks.extend(shared.backend.diagnose().await);
    checks.push(engine_api(shared).await);
    let pipe_served = checks
        .iter()
        .any(|check| check.id == DEFAULT_PIPE_CHECK && check.status == CheckStatus::Pass);
    checks.extend(docker_context(shared, pipe_served));
    checks.extend(cli_tools(shared).await);
    checks.push(disk_space(shared).await);

    checks
}

/// The bundle's zip; `step` hears each stage as it starts.
async fn write_bundle(shared: &Shared, redactor: Redactor, step: impl Fn(&str)) -> Result<Vec<u8>, BundleError> {
    let mut bundle = Bundle::new(redactor);
    bundle.add_json(
        "contrd.json",
        json!({
            "version": contr_core::product::VERSION,
            "apiRevision": contr_api::API_REVISION,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "backend": format!("{:?}", shared.backend_kind),
            "capabilities": shared.capabilities.iter().map(|capability| format!("{capability:?}")).collect::<Vec<_>>(),
            "engineState": format!("{:?}", shared.supervisor.status().state()),
            "uptimeSeconds": shared.started_at.elapsed().map(|uptime| uptime.as_secs()).unwrap_or_default(),
        }),
    )?;

    step("Running the checks");
    let checks = run_all_checks(shared).await;
    bundle.add_json("checks.json", checks.iter().map(check_json).collect())?;

    step("Reading the settings");
    let settings = shared.settings.snapshot();
    bundle.add_json(
        "settings.json",
        json!({
            "effective": serde_json::to_value(&settings.effective.configured).unwrap_or_default(),
            "lockedByAdministrator": settings.effective.locked,
            "composeProjects": serde_json::to_value(&settings.compose_projects).unwrap_or_default(),
        }),
    )?;

    step("Asking the engine");
    match shared.slot.get().await {
        Ok(handle) => {
            match handle.engine.system_report().await {
                Ok((version, info)) => {
                    bundle.add_json("engine/version.json", version)?;
                    bundle.add_json("engine/info.json", info)?;
                }
                Err(error) => bundle.add_text("engine/error.txt", &error.to_string())?,
            }
            let containers = handle.cache.containers().iter().map(container_json).collect();
            bundle.add_json("engine/containers.json", containers)?;
        }
        Err(status) => bundle.add_text("engine/error.txt", status.message())?,
    }
    bundle.add_json("kubernetes.json", cluster_json(&shared.kubernetes.cluster()))?;

    step("Collecting the logs");
    for file in shared.backend.diagnostic_files().await {
        bundle.add_text(&format!("backend/{}", file.name), &file.text)?;
    }
    if let Some(log) = &shared.log {
        bundle.add_text("logs/contrd.log", &log.contents())?;
    }

    bundle.finish()
}

fn check_json(check: &Check) -> Value {
    json!({
        "id": check.id,
        "title": check.title,
        "status": format!("{:?}", check.status),
        "detail": check.detail,
        "remediations": check.remediations.iter().map(|remediation| remediation.action_id).collect::<Vec<_>>(),
    })
}

/// What a container is, without its command or environment, which may hold secrets.
fn container_json(container: &contr_api::containers::v1::Container) -> Value {
    json!({
        "id": container.id.chars().take(12).collect::<String>(),
        "names": container.names,
        "image": container.image,
        "state": format!("{:?}", container.state()),
        "status": container.status,
        "composeProject": container.labels.get(COMPOSE_PROJECT_LABEL),
    })
}

fn cluster_json(cluster: &contr_api::kubernetes::v1::Cluster) -> Value {
    json!({
        "state": format!("{:?}", cluster.state()),
        "version": cluster.version,
        "apiServer": cluster.api_server,
        "kubeconfig": cluster.kubeconfig,
        "context": cluster.context,
        "error": cluster.error,
        "steps": cluster.steps.iter().map(|step| json!({
            "kind": format!("{:?}", step.kind()),
            "status": format!("{:?}", step.status()),
            "detail": step.detail,
        })).collect::<Vec<_>>(),
        "systemPods": cluster.system_pods.iter().map(|pod| json!({
            "name": pod.name,
            "phase": pod.phase,
            "ready": format!("{}/{}", pod.ready_containers, pod.containers),
        })).collect::<Vec<_>>(),
    })
}

/// Reports a remediation's steps as the operation's events and steps.
struct RemediationSink(Sink);

impl ProgressSink for RemediationSink {
    fn report(&self, progress: Progress) {
        let progress = common::Progress {
            message: progress.message,
            fraction: progress.fraction,
        };
        self.0.progress(progress.clone());
        self.0
            .emit(OperationEvent::RunRemediation(proto::RunRemediationResponse {
                event: Some(Event::Progress(progress)),
            }));
    }
}

async fn prerequisites(backend: &dyn Backend) -> Check {
    const ID: &str = "prerequisites";
    const TITLE: &str = "Engine prerequisites";

    match backend.probe().await {
        Ok(probe) if probe.is_ready() => {
            Check::new(ID, TITLE, CheckStatus::Pass, "Everything the engine needs is in place.")
        }
        Ok(probe) => {
            let detail = probe
                .blockers
                .iter()
                .map(|blocker| blocker.reason.description())
                .collect::<Vec<_>>()
                .join(" ");
            Check {
                remediations: probe
                    .blockers
                    .into_iter()
                    .flat_map(|blocker| blocker.remediations)
                    .collect(),
                ..Check::new(ID, TITLE, CheckStatus::Fail, detail)
            }
        }
        Err(error) => Check::new(ID, TITLE, CheckStatus::Fail, error.to_string()),
    }
}

async fn engine_api(shared: &Shared) -> Check {
    const ID: &str = "engine-api";
    const TITLE: &str = "Engine API";

    match shared.supervisor.status().state() {
        EngineState::Running => match shared.slot.get().await {
            Ok(handle) => Check::new(
                ID,
                TITLE,
                CheckStatus::Pass,
                format!(
                    "Engine {} answers at API version {}.",
                    handle.info.version, handle.info.api_version
                ),
            ),
            Err(status) => Check::new(ID, TITLE, CheckStatus::Fail, status.message().to_owned()),
        },
        EngineState::Failed(reason) => Check::new(ID, TITLE, CheckStatus::Fail, reason.description()),
        state => Check::new(
            ID,
            TITLE,
            CheckStatus::Skip,
            format!("The engine is {}.", format!("{state:?}").to_lowercase()),
        ),
    }
}

fn docker_context(shared: &Shared, default_reaches_engine: bool) -> Option<Check> {
    const ID: &str = "docker-context";
    const TITLE: &str = "Docker CLI context";

    let managed = shared.docker_context.as_ref()?;
    let check = match contr_platform::docker_context::current(&managed.config_dir) {
        Ok(current) if current == managed.name => Check::new(
            ID,
            TITLE,
            CheckStatus::Pass,
            format!("docker uses the {current} context."),
        ),
        Ok(current) if current == DEFAULT_CONTEXT && default_reaches_engine => Check::new(
            ID,
            TITLE,
            CheckStatus::Pass,
            "docker uses the default context, which reaches this engine through the standard Docker pipe.",
        ),
        Ok(current) => Check::new(
            ID,
            TITLE,
            CheckStatus::Warn,
            format!(
                "docker uses the {current} context; switch to {} or pass --context {} to reach this engine.",
                managed.name, managed.name
            ),
        ),
        Err(error) => Check::new(
            ID,
            TITLE,
            CheckStatus::Warn,
            format!("Cannot read the Docker CLI configuration: {error}"),
        ),
    };

    Some(check)
}

async fn cli_tools(shared: &Shared) -> Option<Check> {
    const ID: &str = "cli-tools";
    const TITLE: &str = "Docker CLI tools";

    let locations = shared.cli.clone()?;
    let tools = tokio::task::spawn_blocking(move || super::integration::cli_tools(&locations, false))
        .await
        .ok()?
        .ok()?;
    let installed = tools
        .tools
        .iter()
        .find(|tool| tool.name == "docker")
        .map(|tool| &tool.installed_version);

    let check = match installed.filter(|version| !version.is_empty()) {
        None => Check::new(
            ID,
            TITLE,
            CheckStatus::Warn,
            "Not installed; install them from Settings or with `contr cli install`.",
        ),
        Some(version) if tools.on_path => Check::new(
            ID,
            TITLE,
            CheckStatus::Pass,
            format!("docker {version} runs from {}.", tools.bin_dir),
        ),
        Some(_) if !tools.shadowed_by.is_empty() => Check::new(
            ID,
            TITLE,
            CheckStatus::Warn,
            format!("New shells run {} instead of Contr's docker.", tools.shadowed_by),
        ),
        Some(_) => Check::new(
            ID,
            TITLE,
            CheckStatus::Warn,
            format!("{} is not on PATH; run `contr cli install` to add it.", tools.bin_dir),
        ),
    };

    Some(check)
}

async fn disk_space(shared: &Shared) -> Check {
    const ID: &str = "disk-space";
    const TITLE: &str = "Free disk space";

    let Some(path) = data_location(shared).await else {
        return Check::new(
            ID,
            TITLE,
            CheckStatus::Skip,
            "The engine's data location is unknown while it is stopped.",
        );
    };
    let free = match tokio::task::spawn_blocking({
        let path = path.clone();
        move || contr_platform::disk::free_bytes(&path)
    })
    .await
    {
        Ok(Ok(free)) => free,
        Ok(Err(error)) => {
            return Check::new(
                ID,
                TITLE,
                CheckStatus::Skip,
                format!("Cannot measure {}: {error}", path.display()),
            );
        }
        Err(error) => return Check::new(ID, TITLE, CheckStatus::Skip, error.to_string()),
    };

    let status = if free < DISK_FAIL_BELOW {
        CheckStatus::Fail
    } else if free < DISK_WARN_BELOW {
        CheckStatus::Warn
    } else {
        CheckStatus::Pass
    };
    Check::new(
        ID,
        TITLE,
        status,
        format!("{} GiB free where engine data lives ({}).", free / GIB, path.display()),
    )
}

/// Nearest existing directory of the file system that holds the engine's data, as this machine sees it.
#[cfg(windows)]
#[allow(clippy::unused_async, reason = "platforms that ask the engine share the signature")]
async fn data_location(_shared: &Shared) -> Option<PathBuf> {
    let dir = contr_core::paths::wsl_dir()?;
    dir.ancestors().find(|dir| dir.exists()).map(PathBuf::from)
}

/// Nearest existing directory of the file system that holds the engine's data, as this machine sees it.
#[cfg(not(windows))]
async fn data_location(shared: &Shared) -> Option<PathBuf> {
    let handle = shared.slot.get().await.ok()?;
    let root = PathBuf::from(handle.engine.root_dir().await.ok()?);
    root.ancestors().find(|dir| dir.exists()).map(PathBuf::from)
}

fn check(check: Check) -> proto::Check {
    let status = match check.status {
        CheckStatus::Pass => proto::CheckStatus::Pass,
        CheckStatus::Warn => proto::CheckStatus::Warn,
        CheckStatus::Fail => proto::CheckStatus::Fail,
        CheckStatus::Skip => proto::CheckStatus::Skip,
    };

    proto::Check {
        id: check.id.to_owned(),
        title: check.title.to_owned(),
        status: status.into(),
        detail: check.detail,
        remediations: check.remediations.iter().map(remediation).collect(),
    }
}

fn remediation(remediation: &Remediation) -> common::Remediation {
    common::Remediation {
        action_id: remediation.action_id.to_owned(),
        requires_elevation: remediation.requires_elevation,
    }
}
