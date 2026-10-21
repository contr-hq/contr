use clap::Subcommand;
use contr_sdk::api::settings::v1::{
    EngineSettings, GetSettingsRequest, ResetSettingsRequest, Settings, SettingsState, UpdateSettingsRequest,
};
use contr_sdk::{Client, Error};

use crate::format;

#[derive(Subcommand)]
pub(crate) enum SettingsCommand {
    /// Show every setting and which ones your organization manages.
    Show,
    /// Change one setting, such as `engine.start_with_daemon true`.
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
    let engine = match path {
        "engine.start_with_daemon" => EngineSettings {
            start_with_daemon: flag()?,
            ..Default::default()
        },
        "engine.serve_default_pipe" => EngineSettings {
            serve_default_pipe: flag()?,
            ..Default::default()
        },
        _ => anyhow::bail!("{path} is not a setting"),
    };

    Ok(Settings { engine: Some(engine) })
}

fn print(state: &SettingsState) {
    let engine = state.settings.unwrap_or_default().engine.unwrap_or_default();
    let rows: Vec<Vec<String>> = [
        ("engine.start_with_daemon", engine.start_with_daemon),
        ("engine.serve_default_pipe", engine.serve_default_pipe),
    ]
    .into_iter()
    .map(|(path, value)| {
        let note = if state.locked_paths.iter().any(|locked| locked == path) {
            "managed by your organization"
        } else {
            ""
        };
        vec![path.to_owned(), value.to_string(), note.to_owned()]
    })
    .collect();

    print!("{}", format::table(&["SETTING", "VALUE", ""], &rows));
}
