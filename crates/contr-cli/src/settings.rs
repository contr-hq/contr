use anyhow::Context;
use clap::Subcommand;
use contr_sdk::api::settings::v1::{
    CertificateSettings, EngineSettings, GetSettingsRequest, KubernetesSettings, ProxyMode, ProxySettings,
    RegistrySettings, ResetSettingsRequest, Settings, SettingsState, UpdateSettingsRequest,
};
use contr_sdk::{Client, Error};

use crate::format;

#[derive(Subcommand)]
pub(crate) enum SettingsCommand {
    /// Show every setting and which ones your organization manages.
    Show,
    /// Change one setting, such as `engine.start_with_daemon true`, `registry.mirrors https://a,https://b` or
    /// `engine.daemon_json @daemon.json`.
    Set { path: String, value: String },
    /// Return every setting you changed to its default.
    Reset,
}

pub(crate) async fn run(client: &Client, command: Option<SettingsCommand>) -> anyhow::Result<i32> {
    let mut settings = client.settings();
    let (state, restart_required) = match command.unwrap_or(SettingsCommand::Show) {
        SettingsCommand::Show => {
            let response = settings
                .get_settings(GetSettingsRequest {})
                .await
                .map_err(Error::from)?;
            (response.into_inner().state, Vec::new())
        }
        SettingsCommand::Set { path, value } => {
            let request = UpdateSettingsRequest {
                settings: Some(with_value(&path, &value)?),
                update_mask: Some(prost_types::FieldMask { paths: vec![path] }),
                etag: String::new(),
            };
            let response = match settings.update_settings(request).await {
                Ok(response) => response.into_inner(),
                Err(status) => {
                    let error = Error::from(status);
                    explain_unknown_keys(&error);
                    return Err(error.into());
                }
            };
            (response.state, response.restart_required)
        }
        SettingsCommand::Reset => {
            let response = settings
                .reset_settings(ResetSettingsRequest {})
                .await
                .map_err(Error::from)?
                .into_inner();
            (response.state, response.restart_required)
        }
    };

    print(&state.unwrap_or_default());
    for path in restart_required {
        println!("{path} applies when the engine next starts.");
    }
    Ok(0)
}

/// Names each `daemon.json` key the engine does not know, with the key it takes instead.
fn explain_unknown_keys(error: &Error) {
    let Some(rejection) = error.daemon_json_rejection() else {
        return;
    };
    for unknown in rejection.unknown_keys {
        if unknown.suggestion.is_empty() {
            eprintln!("{} is not an engine option.", unknown.key);
        } else {
            eprintln!(
                "{} is not an engine option; did you mean {}?",
                unknown.key, unknown.suggestion
            );
        }
    }
}

/// Settings holding `value` at `path`, for an update whose mask names only `path`.
fn with_value(path: &str, value: &str) -> anyhow::Result<Settings> {
    let flag = || match value {
        "true" | "on" | "yes" => Ok(true),
        "false" | "off" | "no" => Ok(false),
        _ => anyhow::bail!("{path} is true or false, not {value:?}"),
    };
    let mut settings = Settings::default();
    match path {
        "engine.start_with_daemon" => {
            settings.engine = Some(EngineSettings {
                start_with_daemon: flag()?,
                ..Default::default()
            });
        }
        "engine.serve_default_pipe" => {
            settings.engine = Some(EngineSettings {
                serve_default_pipe: flag()?,
                ..Default::default()
            });
        }
        "engine.daemon_json" => {
            let daemon_json = match value.strip_prefix('@') {
                Some(file) => std::fs::read_to_string(file).with_context(|| format!("cannot read {file}"))?,
                None => value.to_owned(),
            };
            settings.engine = Some(EngineSettings {
                daemon_json,
                ..Default::default()
            });
        }
        "proxy.mode" => {
            let mode = PROXY_MODES
                .iter()
                .find(|(name, _)| *name == value)
                .map(|(_, mode)| *mode)
                .ok_or_else(|| anyhow::anyhow!("proxy.mode is system, manual or none, not {value:?}"))?;
            settings.proxy = Some(ProxySettings {
                mode: mode.into(),
                ..Default::default()
            });
        }
        "proxy.http" | "proxy.https" | "proxy.exclude" => {
            let mut proxy = ProxySettings::default();
            match path {
                "proxy.http" => &mut proxy.http,
                "proxy.https" => &mut proxy.https,
                _ => &mut proxy.exclude,
            }
            .push_str(value);
            settings.proxy = Some(proxy);
        }
        "proxy.pass_to_containers" => {
            settings.proxy = Some(ProxySettings {
                pass_to_containers: flag()?,
                ..Default::default()
            });
        }
        "certificates.trust_system" => {
            settings.certificates = Some(CertificateSettings { trust_system: flag()? });
        }
        "registry.mirrors" => {
            settings.registry = Some(RegistrySettings {
                mirrors: value
                    .split(',')
                    .map(str::trim)
                    .filter(|mirror| !mirror.is_empty())
                    .map(str::to_owned)
                    .collect(),
            });
        }
        "kubernetes.enabled" | "kubernetes.traefik" => {
            let mut kubernetes = KubernetesSettings::default();
            *if path == "kubernetes.enabled" {
                &mut kubernetes.enabled
            } else {
                &mut kubernetes.traefik
            } = flag()?;
            settings.kubernetes = Some(kubernetes);
        }
        "kubernetes.version" => {
            settings.kubernetes = Some(KubernetesSettings {
                version: value.to_owned(),
                ..Default::default()
            });
        }
        _ => anyhow::bail!("{path} is not a setting"),
    }

    Ok(settings)
}

/// Names `contr settings set proxy.mode` takes.
const PROXY_MODES: [(&str, ProxyMode); 3] = [
    ("system", ProxyMode::System),
    ("manual", ProxyMode::Manual),
    ("none", ProxyMode::None),
];

fn print(state: &SettingsState) {
    let settings = state.settings.clone().unwrap_or_default();
    let engine = settings.engine.unwrap_or_default();
    let proxy = settings.proxy.unwrap_or_default();
    let mode = PROXY_MODES
        .iter()
        .find(|(_, mode)| *mode == proxy.mode())
        .map_or("system", |(name, _)| name);
    let daemon_json = serde_json::from_str::<serde_json::Value>(&engine.daemon_json)
        .map_or(engine.daemon_json.clone(), |daemon| daemon.to_string());
    let kubernetes = settings.kubernetes.clone().unwrap_or_default();
    let rows: Vec<Vec<String>> = [
        ("engine.start_with_daemon", engine.start_with_daemon.to_string()),
        ("engine.serve_default_pipe", engine.serve_default_pipe.to_string()),
        ("engine.daemon_json", daemon_json),
        ("proxy.mode", mode.to_owned()),
        ("proxy.http", proxy.http),
        ("proxy.https", proxy.https),
        ("proxy.exclude", proxy.exclude),
        ("proxy.pass_to_containers", proxy.pass_to_containers.to_string()),
        (
            "certificates.trust_system",
            settings.certificates.unwrap_or_default().trust_system.to_string(),
        ),
        (
            "registry.mirrors",
            settings.registry.unwrap_or_default().mirrors.join(","),
        ),
        ("kubernetes.enabled", kubernetes.enabled.to_string()),
        ("kubernetes.version", kubernetes.version.clone()),
        ("kubernetes.traefik", kubernetes.traefik.to_string()),
    ]
    .into_iter()
    .map(|(path, value)| {
        let note = if state.locked_paths.iter().any(|locked| locked == path) {
            "managed by your organization"
        } else {
            ""
        };
        vec![path.to_owned(), value, note.to_owned()]
    })
    .collect();

    print!("{}", format::table(&["SETTING", "VALUE", ""], &rows));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passing_the_proxy_to_containers_takes_a_flag() {
        let settings = with_value("proxy.pass_to_containers", "on").unwrap();

        assert!(settings.proxy.unwrap().pass_to_containers);
        assert!(with_value("proxy.pass_to_containers", "sometimes").is_err());
    }
}
