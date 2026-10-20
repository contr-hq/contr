use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use contr_api::integration::v1 as proto;
use contr_api::integration::v1::integration_service_server::IntegrationService;
use contr_backend::{IntegrationState, WslDistribution, WslIntegration};
use contr_platform::cli_tools::{Bundle, Installation};
use contr_settings::{SettingsError, SettingsFile};
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

use super::Shared;
use crate::status;

/// Where the daemon finds its bundled Docker CLI tools and installs them for the user.
#[derive(Clone, Debug)]
pub(crate) struct CliLocations {
    pub(crate) bundle: PathBuf,
    pub(crate) installation: Installation,
}

/// Engine access from the user's WSL distributions and the settings file that keeps it across restarts.
pub(crate) struct WslAccess {
    pub(crate) integration: Arc<dyn WslIntegration>,
    pub(crate) settings: Mutex<SettingsFile>,
}

pub(crate) struct IntegrationApi(pub(crate) Arc<Shared>);

#[tonic::async_trait]
impl IntegrationService for IntegrationApi {
    async fn get_cli_tools(
        &self,
        _: Request<proto::GetCliToolsRequest>,
    ) -> Result<Response<proto::GetCliToolsResponse>, Status> {
        Ok(Response::new(proto::GetCliToolsResponse {
            cli_tools: Some(self.cli_tools(false).await?),
        }))
    }

    async fn install_cli_tools(
        &self,
        _: Request<proto::InstallCliToolsRequest>,
    ) -> Result<Response<proto::InstallCliToolsResponse>, Status> {
        Ok(Response::new(proto::InstallCliToolsResponse {
            cli_tools: Some(self.cli_tools(true).await?),
        }))
    }

    async fn list_wsl_distributions(
        &self,
        _: Request<proto::ListWslDistributionsRequest>,
    ) -> Result<Response<proto::ListWslDistributionsResponse>, Status> {
        let distributions = distributions(self.wsl()?).await?;

        Ok(Response::new(proto::ListWslDistributionsResponse {
            distributions: distributions.into_iter().map(wsl_distribution).collect(),
        }))
    }

    async fn set_wsl_integration(
        &self,
        request: Request<proto::SetWslIntegrationRequest>,
    ) -> Result<Response<proto::SetWslIntegrationResponse>, Status> {
        let wsl = self.wsl()?;
        let request = request.into_inner();
        let distribution = registered(wsl, &request.name).await?;
        if distribution.version < 2 {
            return Err(Status::failed_precondition(format!(
                "{} runs on WSL 1; convert it with `wsl --set-version {} 2` first",
                distribution.name, distribution.name
            )));
        }

        remember(wsl, &distribution.name, request.enabled).await?;
        wsl.integration
            .set_integrated(&distribution.name, request.enabled)
            .await;

        Ok(Response::new(proto::SetWslIntegrationResponse {
            distribution: Some(wsl_distribution(registered(wsl, &distribution.name).await?)),
        }))
    }
}

impl IntegrationApi {
    fn wsl(&self) -> Result<&WslAccess, Status> {
        self.0
            .wsl
            .as_ref()
            .ok_or_else(|| status::feature_unavailable("the engine does not run in WSL"))
    }

    async fn cli_tools(&self, install: bool) -> Result<proto::CliTools, Status> {
        let Some(locations) = self.0.cli.clone() else {
            if install {
                return Err(status::feature_unavailable("this daemon bundles no Docker CLI tools"));
            }
            return Ok(proto::CliTools::default());
        };

        tokio::task::spawn_blocking(move || cli_tools(&locations, install))
            .await
            .map_err(|error| Status::internal(error.to_string()))?
    }
}

async fn distributions(wsl: &WslAccess) -> Result<Vec<WslDistribution>, Status> {
    wsl.integration
        .distributions()
        .await
        .map_err(|error| Status::internal(format!("cannot list the WSL distributions: {error}")))
}

/// The registered distribution `name`, which WSL compares without regard to case.
async fn registered(wsl: &WslAccess, name: &str) -> Result<WslDistribution, Status> {
    distributions(wsl)
        .await?
        .into_iter()
        .find(|distribution| distribution.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| Status::not_found(format!("no WSL distribution named {name:?} is registered")))
}

async fn remember(wsl: &WslAccess, name: &str, integrated: bool) -> Result<(), Status> {
    let file = wsl.settings.lock().await;
    let mut settings = file.load().map_err(|error| settings_unusable(&error))?;
    let changed = if integrated {
        settings.wsl_integrations.insert(name.to_owned())
    } else {
        settings.wsl_integrations.remove(name)
    };
    if changed {
        file.save(&settings).map_err(|error| settings_unusable(&error))?;
    }

    Ok(())
}

fn settings_unusable(error: &SettingsError) -> Status {
    match std::error::Error::source(error) {
        Some(cause) => Status::internal(format!("{error}: {cause}")),
        None => Status::internal(error.to_string()),
    }
}

fn wsl_distribution(distribution: WslDistribution) -> proto::WslDistribution {
    let (state, failure) = match distribution.integration {
        IntegrationState::Disabled => (proto::WslIntegrationState::Disabled, String::new()),
        IntegrationState::Pending => (proto::WslIntegrationState::Pending, String::new()),
        IntegrationState::Active => (proto::WslIntegrationState::Active, String::new()),
        IntegrationState::Failed(reason) => (proto::WslIntegrationState::Failed, reason),
    };

    proto::WslDistribution {
        name: distribution.name,
        wsl_version: u32::from(distribution.version),
        is_default: distribution.is_default,
        integration_state: state.into(),
        failure,
    }
}

pub(super) fn cli_tools(locations: &CliLocations, install: bool) -> Result<proto::CliTools, Status> {
    let bundle = Bundle::load(&locations.bundle);
    if install {
        let bundle = bundle
            .as_ref()
            .map_err(|error| status::feature_unavailable(&format!("the Docker CLI bundle is unusable: {error}")))?;
        install_for_user(bundle, &locations.installation)
            .map_err(|error| Status::internal(format!("cannot install the Docker CLI tools: {error}")))?;
    }

    let bundled = bundle.map(|bundle| bundle.versions().clone()).unwrap_or_default();
    let installed = locations.installation.versions();
    let names: BTreeSet<&String> = bundled.keys().chain(installed.keys()).collect();
    let tools = names
        .into_iter()
        .map(|name| proto::CliTool {
            name: name.clone(),
            bundled_version: bundled.get(name).cloned().unwrap_or_default(),
            installed_version: installed.get(name).cloned().unwrap_or_default(),
        })
        .collect();

    let bin_dir = locations.installation.bin_dir();
    let (on_path, shadowed_by) = resolution(&bin_dir);
    Ok(proto::CliTools {
        tools,
        bin_dir: bin_dir.display().to_string(),
        on_path,
        shadowed_by,
    })
}

#[cfg(windows)]
fn install_for_user(bundle: &Bundle, installation: &Installation) -> std::io::Result<()> {
    installation.install_from(bundle)?;
    if let Some(docker_config) = contr_platform::docker_context::config_dir() {
        contr_platform::cli_tools::register_with_docker(&docker_config, &installation.plugins_dir(), "wincred")?;
    }
    contr_platform::user_path::add(&installation.bin_dir())?;

    Ok(())
}

#[cfg(not(windows))]
fn install_for_user(bundle: &Bundle, installation: &Installation) -> std::io::Result<()> {
    installation.install_from(bundle)
}

/// Whether a new shell finds the installed `docker` first, and which other `docker` it would find instead.
#[cfg(windows)]
fn resolution(bin_dir: &std::path::Path) -> (bool, String) {
    match contr_platform::user_path::first_holding("docker.exe") {
        Some(dir) if contr_platform::user_path::same_dir(&dir.to_string_lossy(), bin_dir) => (true, String::new()),
        Some(dir) => (false, dir.join("docker.exe").display().to_string()),
        None => (false, String::new()),
    }
}

#[cfg(not(windows))]
fn resolution(_bin_dir: &std::path::Path) -> (bool, String) {
    (false, String::new())
}
