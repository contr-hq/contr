use std::sync::Arc;

use contr_api::settings::v1 as proto;
use contr_api::settings::v1::settings_service_server::SettingsService;
use contr_backend::EngineState;
use contr_settings::Configured;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio_stream::wrappers::WatchStream;
use tonic::{Request, Response, Status};

use super::{ResponseStream, Shared};
use crate::settings::Snapshot;
use crate::status;

/// Paths whose change applies at the next engine start.
const ENGINE_RESTART_PATHS: [&str; 1] = ["engine.serve_default_pipe"];

pub(crate) struct SettingsApi(pub(crate) Arc<Shared>);

#[tonic::async_trait]
impl SettingsService for SettingsApi {
    type WatchSettingsStream = ResponseStream<proto::WatchSettingsResponse>;

    async fn get_settings(
        &self,
        _: Request<proto::GetSettingsRequest>,
    ) -> Result<Response<proto::GetSettingsResponse>, Status> {
        Ok(Response::new(proto::GetSettingsResponse {
            state: Some(state(&self.0.settings.snapshot())),
        }))
    }

    async fn watch_settings(
        &self,
        _: Request<proto::WatchSettingsRequest>,
    ) -> Result<Response<Self::WatchSettingsStream>, Status> {
        let states = WatchStream::new(self.0.settings.subscribe()).map(|snapshot| {
            Ok(proto::WatchSettingsResponse {
                state: Some(state(&snapshot)),
            })
        });

        Ok(Response::new(states.boxed()))
    }

    async fn update_settings(
        &self,
        request: Request<proto::UpdateSettingsRequest>,
    ) -> Result<Response<proto::UpdateSettingsResponse>, Status> {
        let request = request.into_inner();
        let settings = request.settings.unwrap_or_default();
        let paths = request.update_mask.map(|mask| mask.paths).unwrap_or_default();
        let mut values = Vec::with_capacity(paths.len());
        for path in &paths {
            let value =
                value(settings, path).ok_or_else(|| Status::invalid_argument(format!("{path} is not a setting")))?;
            values.push((path.as_str(), value));
        }
        let locked = self.0.settings.snapshot().effective.locked;
        if let Some((path, _)) = values.iter().find(|(path, _)| locked.contains(path)) {
            return Err(status::setting_locked(path));
        }
        let expected = Some(request.etag)
            .filter(|etag| !etag.is_empty())
            .map(|etag| {
                etag.parse::<u64>()
                    .map_err(|_| Status::invalid_argument("the etag is not one the daemon gave"))
            })
            .transpose()?;

        let (changed, snapshot) = self
            .0
            .settings
            .update(expected, self.0.backend.as_ref(), |stored| {
                let mut changed = Vec::new();
                for (path, value) in values {
                    if stored
                        .set(path, value)
                        .map_err(|error| crate::settings::unusable(&error))?
                    {
                        changed.push(path.to_owned());
                    }
                }
                Ok(changed)
            })
            .await?;

        Ok(Response::new(proto::UpdateSettingsResponse {
            state: Some(state(&snapshot)),
            restart_required: self.restart_required(changed.iter().map(String::as_str)),
        }))
    }

    async fn reset_settings(
        &self,
        _: Request<proto::ResetSettingsRequest>,
    ) -> Result<Response<proto::ResetSettingsResponse>, Status> {
        let (cleared, snapshot) = self
            .0
            .settings
            .update(None, self.0.backend.as_ref(), |stored| Ok(stored.clear()))
            .await?;

        Ok(Response::new(proto::ResetSettingsResponse {
            state: Some(state(&snapshot)),
            restart_required: self.restart_required(cleared.into_iter()),
        }))
    }
}

impl SettingsApi {
    /// The `changed` paths that apply at the next engine start, while the engine runs.
    fn restart_required<'a>(&self, changed: impl Iterator<Item = &'a str>) -> Vec<String> {
        if self.0.supervisor.status().state() != EngineState::Running {
            return Vec::new();
        }

        changed
            .filter(|path| ENGINE_RESTART_PATHS.contains(path))
            .map(str::to_owned)
            .collect()
    }
}

fn state(snapshot: &Snapshot) -> proto::SettingsState {
    proto::SettingsState {
        settings: Some(settings(&snapshot.effective.configured)),
        locked_paths: snapshot
            .effective
            .locked
            .iter()
            .map(|path| (*path).to_owned())
            .collect(),
        etag: snapshot.revision.to_string(),
    }
}

fn settings(configured: &Configured) -> proto::Settings {
    proto::Settings {
        engine: Some(proto::EngineSettings {
            start_with_daemon: configured.engine.start_with_daemon,
            serve_default_pipe: configured.engine.serve_default_pipe,
        }),
    }
}

/// The value `settings` holds at `path`, or `None` for a path that names no setting.
fn value(settings: proto::Settings, path: &str) -> Option<Value> {
    let engine = settings.engine.unwrap_or_default();
    match path {
        "engine.start_with_daemon" => Some(json!(engine.start_with_daemon)),
        "engine.serve_default_pipe" => Some(json!(engine.serve_default_pipe)),
        _ => None,
    }
}
