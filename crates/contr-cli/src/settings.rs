use clap::Subcommand;
use contr_sdk::api::settings::v1::{
    CertificateSettings, EngineSettings, GetSettingsRequest, ProxyMode, ProxySettings, RegistrySettings,
    ResetSettingsRequest, Settings, SettingsState, UpdateSettingsRequest,
};
use contr_sdk::{Client, Error};

use crate::format;

#[derive(Subcommand)]
pub(crate) enum SettingsCommand {
    /// Show every setting and which ones your organization manages.
    Show,
    /// Change one setting, such as `engine.start_with_daemon true` or `registry.mirrors https://a,https://b`.
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
            let response = settings
                .update_settings(request)
                .await
                .map_err(Error::from)?
                .into_inner();
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
    let rows: Vec<Vec<String>> = [
        ("engine.start_with_daemon", engine.start_with_daemon.to_string()),
        ("engine.serve_default_pipe", engine.serve_default_pipe.to_string()),
        ("proxy.mode", mode.to_owned()),
        ("proxy.http", proxy.http),
        ("proxy.https", proxy.https),
        ("proxy.exclude", proxy.exclude),
        (
            "certificates.trust_system",
            settings.certificates.unwrap_or_default().trust_system.to_string(),
        ),
        (
            "registry.mirrors",
            settings.registry.unwrap_or_default().mirrors.join(","),
        ),
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
