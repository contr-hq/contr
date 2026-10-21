use std::path::PathBuf;
use std::sync::Arc;

use contr_api::common::v1 as common;
use contr_api::diagnostics::v1 as proto;
use contr_api::diagnostics::v1::diagnostics_service_server::DiagnosticsService;
use contr_api::diagnostics::v1::run_remediation_response::Event;
use contr_backend::{Backend, BackendError, Check, CheckStatus, EngineState, Progress, ProgressSink};
use contr_core::Remediation;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use super::{ResponseStream, Shared};

const GIB: u64 = 1024 * 1024 * 1024;
const DISK_FAIL_BELOW: u64 = 2 * GIB;
const DISK_WARN_BELOW: u64 = 10 * GIB;
const EVENT_QUEUE: usize = 32;
/// Check of the backends that serve the Docker CLI's default endpoint, which the `default` context reaches.
const DEFAULT_PIPE_CHECK: &str = "default-pipe";
const DEFAULT_CONTEXT: &str = "default";

pub(crate) struct DiagnosticsApi(pub(crate) Arc<Shared>);

#[tonic::async_trait]
impl DiagnosticsService for DiagnosticsApi {
    type RunRemediationStream = ResponseStream<proto::RunRemediationResponse>;

    async fn run_checks(
        &self,
        _: Request<proto::RunChecksRequest>,
    ) -> Result<Response<proto::RunChecksResponse>, Status> {
        let shared = &self.0;
        let mut checks = vec![prerequisites(shared.backend.as_ref()).await];
        checks.extend(shared.backend.diagnose().await);
        checks.push(engine_api(shared).await);
        let pipe_served = checks
            .iter()
            .any(|check| check.id == DEFAULT_PIPE_CHECK && check.status == CheckStatus::Pass);
        checks.extend(docker_context(shared, pipe_served));
        checks.extend(cli_tools(shared).await);
        checks.push(disk_space(shared).await);

        Ok(Response::new(proto::RunChecksResponse {
            checks: checks.into_iter().map(check).collect(),
        }))
    }

    async fn run_remediation(
        &self,
        request: Request<proto::RunRemediationRequest>,
    ) -> Result<Response<Self::RunRemediationStream>, Status> {
        let action_id = request.into_inner().action_id;
        let remediation = self
            .0
            .backend
            .remediations()
            .iter()
            .find(|remediation| remediation.action_id == action_id)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("this backend offers no remediation {action_id:?}")))?;

        let (events, receiver) = mpsc::channel(EVENT_QUEUE);
        let backend = Arc::clone(&self.0.backend);
        tokio::spawn(async move {
            let sink = ChannelSink(events.clone());
            let finished = match backend.remediate(&remediation, &sink).await {
                Ok(outcome) => Ok(proto::RunRemediationResponse {
                    event: Some(Event::Outcome(proto::RemediationOutcome {
                        restart_required: outcome.restart_required,
                        logout_required: outcome.logout_required,
                    })),
                }),
                Err(error @ BackendError::Dismissed) => Err(Status::cancelled(format!(
                    "{} did not run: {error}",
                    remediation.action_id
                ))),
                Err(error) => Err(Status::failed_precondition(format!(
                    "{} failed: {error}",
                    remediation.action_id
                ))),
            };
            // The caller may have dropped the stream; the remediation still ran.
            let _ = events.send(finished).await;
        });

        Ok(Response::new(ReceiverStream::new(receiver).boxed()))
    }
}

struct ChannelSink(mpsc::Sender<Result<proto::RunRemediationResponse, Status>>);

impl ProgressSink for ChannelSink {
    fn report(&self, progress: Progress) {
        let _ = self.0.try_send(Ok(proto::RunRemediationResponse {
            event: Some(Event::Progress(common::Progress {
                message: progress.message,
                fraction: progress.fraction,
            })),
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
