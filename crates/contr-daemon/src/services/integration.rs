use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use contr_api::integration::v1 as proto;
use contr_api::integration::v1::integration_service_server::IntegrationService;
use contr_platform::cli_tools::{Bundle, Installation};
use tonic::{Request, Response, Status};

use super::Shared;
use crate::status;

/// Where the daemon finds its bundled Docker CLI tools and installs them for the user.
#[derive(Clone, Debug)]
pub(crate) struct CliLocations {
    pub(crate) bundle: PathBuf,
    pub(crate) installation: Installation,
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
}

impl IntegrationApi {
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

fn cli_tools(locations: &CliLocations, install: bool) -> Result<proto::CliTools, Status> {
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
