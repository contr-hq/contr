use std::sync::Arc;

use contr_api::settings::v1 as proto;
use contr_api::settings::v1::settings_service_server::SettingsService;
use contr_backend::EngineState;
use contr_settings::{Configured, ProxyMode};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio_stream::wrappers::WatchStream;
use tonic::{Request, Response, Status};

use super::{ResponseStream, Shared};
use crate::settings::Snapshot;
use crate::status;

/// Paths whose change applies at the next engine start.
const ENGINE_RESTART_PATHS: [&str; 8] = [
    "engine.serve_default_pipe",
    "engine.daemon_json",
    "proxy.mode",
    "proxy.http",
    "proxy.https",
    "proxy.exclude",
    "certificates.trust_system",
    "registry.mirrors",
];

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
            values.push((path.as_str(), value(&settings, path)?));
        }
        let locked = self.0.settings.snapshot().effective.locked;
        if let Some((path, _)) = values.iter().find(|(path, _)| locked.contains(path)) {
            return Err(status::setting_locked(path));
        }
        if let Some((_, Value::Object(daemon))) = values.iter().find(|(path, _)| *path == "engine.daemon_json") {
            self.0
                .backend
                .check_daemon_config(daemon)
                .await
                .map_err(|reason| Status::invalid_argument(format!("engine.daemon_json: {reason}")))?;
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
    let proxy = &configured.proxy;

    proto::Settings {
        engine: Some(proto::EngineSettings {
            start_with_daemon: configured.engine.start_with_daemon,
            serve_default_pipe: configured.engine.serve_default_pipe,
            daemon_json: serde_json::to_string_pretty(&configured.engine.daemon_json).unwrap_or_default(),
        }),
        proxy: Some(proto::ProxySettings {
            mode: match proxy.mode {
                ProxyMode::System => proto::ProxyMode::System,
                ProxyMode::Manual => proto::ProxyMode::Manual,
                ProxyMode::None => proto::ProxyMode::None,
            }
            .into(),
            http: proxy.http.clone(),
            https: proxy.https.clone(),
            exclude: proxy.exclude.clone(),
        }),
        certificates: Some(proto::CertificateSettings {
            trust_system: configured.certificates.trust_system,
        }),
        registry: Some(proto::RegistrySettings {
            mirrors: configured.registry.mirrors.clone(),
        }),
    }
}

/// The value `settings` holds at `path`, as the settings file stores it.
fn value(settings: &proto::Settings, path: &str) -> Result<Value, Status> {
    let engine = settings.engine.clone().unwrap_or_default();
    let proxy = settings.proxy.clone().unwrap_or_default();
    Ok(match path {
        "engine.start_with_daemon" => json!(engine.start_with_daemon),
        "engine.serve_default_pipe" => json!(engine.serve_default_pipe),
        "engine.daemon_json" => daemon_json(&engine.daemon_json)?,
        "proxy.mode" => json!(match proxy.mode() {
            proto::ProxyMode::System => ProxyMode::System,
            proto::ProxyMode::Manual => ProxyMode::Manual,
            proto::ProxyMode::None => ProxyMode::None,
            proto::ProxyMode::Unspecified => return Err(Status::invalid_argument("proxy.mode names no mode")),
        }),
        "proxy.http" => json!(proxy.http),
        "proxy.https" => json!(proxy.https),
        "proxy.exclude" => json!(proxy.exclude),
        "certificates.trust_system" => json!(settings.certificates.unwrap_or_default().trust_system),
        "registry.mirrors" => json!(settings.registry.clone().unwrap_or_default().mirrors),
        _ => return Err(Status::invalid_argument(format!("{path} is not a setting"))),
    })
}

/// The object of a `daemon.json` text; blank text holds no settings.
fn daemon_json(text: &str) -> Result<Value, Status> {
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_str(text) {
        Ok(Value::Object(daemon)) => Ok(Value::Object(daemon)),
        Ok(_) => Err(Status::invalid_argument("engine.daemon_json is not a JSON object")),
        Err(error) => Err(Status::invalid_argument(format!(
            "engine.daemon_json is not JSON: {error}"
        ))),
    }
}
