use clap::Subcommand;
use contr_sdk::api::kubernetes::v1::{
    Cluster, ClusterState, GetClusterRequest, ImportImagesRequest, ResetClusterRequest, SetCurrentContextRequest,
    StepKind, StepStatus, WatchClusterRequest,
};
use contr_sdk::api::settings::v1::{KubernetesSettings, Settings, UpdateSettingsRequest};
use contr_sdk::{Client, Error};

use crate::format;

#[derive(Subcommand)]
pub(crate) enum KubeCommand {
    /// Show the cluster's state.
    Status,
    /// Run the cluster whenever the engine runs, and wait until it is ready.
    Enable {
        /// Kubernetes version, such as v1.36.5; `status` lists the ones available.
        #[arg(long)]
        version: Option<String>,
        /// Run the Traefik ingress controller.
        #[arg(long)]
        traefik: Option<bool>,
    },
    /// Stop the cluster, keeping its data.
    Disable,
    /// Delete the cluster's workloads and data, then start a new cluster; images on the engine stay.
    Reset,
    /// Copy images from the engine into the cluster, so pods use them without a registry.
    Import {
        #[arg(required = true)]
        images: Vec<String>,
    },
    /// Make the cluster's context kubectl's current context.
    UseContext {
        /// Restore the context that was current before instead.
        #[arg(long)]
        off: bool,
    },
}

pub(crate) async fn run(client: &Client, command: KubeCommand) -> anyhow::Result<i32> {
    let mut kubernetes = client.kubernetes();
    match command {
        KubeCommand::Status => {
            let cluster = kubernetes
                .get_cluster(GetClusterRequest {})
                .await
                .map_err(Error::from)?
                .into_inner()
                .cluster
                .unwrap_or_default();
            print_cluster(&cluster);
            Ok(0)
        }
        KubeCommand::Enable { version, traefik } => {
            let mut paths = vec!["kubernetes.enabled".to_owned()];
            if version.is_some() {
                paths.push("kubernetes.version".to_owned());
            }
            if traefik.is_some() {
                paths.push("kubernetes.traefik".to_owned());
            }
            let settings = Settings {
                kubernetes: Some(KubernetesSettings {
                    enabled: true,
                    version: version.unwrap_or_default(),
                    traefik: traefik.unwrap_or_default(),
                }),
                ..Settings::default()
            };
            let etag = update(client, settings, paths).await?;
            follow(client, Some(etag)).await
        }
        KubeCommand::Disable => {
            let settings = Settings {
                kubernetes: Some(KubernetesSettings::default()),
                ..Settings::default()
            };
            update(client, settings, vec!["kubernetes.enabled".to_owned()]).await?;
            Ok(0)
        }
        KubeCommand::Reset => {
            kubernetes
                .reset_cluster(ResetClusterRequest {})
                .await
                .map_err(Error::from)?;
            follow(client, None).await
        }
        KubeCommand::Import { images } => {
            let mut outcomes = kubernetes
                .import_images(ImportImagesRequest { images })
                .await
                .map_err(Error::from)?
                .into_inner();
            let mut failed = false;
            while let Some(outcome) = outcomes.message().await.map_err(Error::from)? {
                if outcome.error.is_empty() {
                    println!("Imported {}.", outcome.image);
                } else {
                    failed = true;
                    eprintln!("Cannot import {}: {}", outcome.image, outcome.error);
                }
            }
            Ok(i32::from(failed))
        }
        KubeCommand::UseContext { off } => {
            let cluster = kubernetes
                .set_current_context(SetCurrentContextRequest { current: !off })
                .await
                .map_err(Error::from)?
                .into_inner()
                .cluster
                .unwrap_or_default();
            match cluster.current_context.as_str() {
                "" => println!("kubectl has no current context."),
                context => println!("kubectl's current context is {context}."),
            }
            Ok(0)
        }
    }
}

/// Applies `settings` at `paths` and returns the etag of the settings that result.
async fn update(client: &Client, settings: Settings, paths: Vec<String>) -> anyhow::Result<u64> {
    let state = client
        .settings()
        .update_settings(UpdateSettingsRequest {
            settings: Some(settings),
            update_mask: Some(prost_types::FieldMask { paths }),
            etag: String::new(),
        })
        .await
        .map_err(Error::from)?
        .into_inner()
        .state
        .unwrap_or_default();

    Ok(state.etag.parse().unwrap_or_default())
}

/// Prints each step as the cluster starts, until it is ready or fails: once it follows the settings with `etag` or
/// later ones, or without an etag once the state that is current when the call starts has passed.
async fn follow(client: &Client, etag: Option<u64>) -> anyhow::Result<i32> {
    let mut states = client
        .kubernetes()
        .watch_cluster(WatchClusterRequest {})
        .await
        .map_err(Error::from)?
        .into_inner();
    let mut printed = 0;
    while let Some(update) = states.message().await.map_err(Error::from)? {
        let cluster = update.cluster.unwrap_or_default();
        let current = etag.is_none_or(|etag| cluster.settings_etag.parse().is_ok_and(|seen: u64| seen >= etag));
        if !current {
            continue;
        }
        let finished: Vec<_> = cluster
            .steps
            .iter()
            .filter(|step| matches!(step.status(), StepStatus::Done | StepStatus::Failed))
            .collect();
        for step in finished.iter().skip(printed) {
            println!("{} {}", step_title(step.kind()), step.detail);
        }
        printed = printed.max(finished.len());
        match cluster.state() {
            ClusterState::Ready => {
                println!("The cluster is ready: kubectl --context {}", cluster.context);
                return Ok(0);
            }
            ClusterState::Failed => {
                eprintln!("The cluster did not start: {}", cluster.error);
                return Ok(1);
            }
            ClusterState::WaitingForEngine => {
                println!("The cluster starts when the engine does.");
                return Ok(0);
            }
            ClusterState::Disabled => {
                println!("The cluster is disabled.");
                return Ok(0);
            }
            _ => {}
        }
    }

    Ok(1)
}

fn step_title(kind: StepKind) -> &'static str {
    match kind {
        StepKind::PullImage => "Pulled the k3s image",
        StepKind::StartServer => "Started the server",
        StepKind::WaitForNode => "The node is ready",
        StepKind::AddKubeconfig => "Added the context to",
        StepKind::Unspecified => "",
    }
}

fn print_cluster(cluster: &Cluster) {
    let state = match cluster.state() {
        ClusterState::Disabled => "disabled",
        ClusterState::WaitingForEngine => "waiting for the engine",
        ClusterState::Starting => "starting",
        ClusterState::Ready => "ready",
        ClusterState::Stopping => "stopping",
        ClusterState::Failed => "failed",
        ClusterState::Unspecified => "unknown",
    };
    println!("Cluster:   {state}");
    if !cluster.version.is_empty() {
        println!("Version:   {}", cluster.version);
    }
    println!("API:       {}", cluster.api_server);
    println!("Context:   {} in {}", cluster.context, cluster.kubeconfig);
    println!(
        "Current:   {}",
        Some(cluster.current_context.as_str())
            .filter(|context| !context.is_empty())
            .unwrap_or("none")
    );
    println!("Versions:  {}", cluster.available_versions.join(", "));
    if !cluster.error.is_empty() {
        println!("Error:     {}", cluster.error);
    }
    if !cluster.system_pods.is_empty() {
        let rows: Vec<Vec<String>> = cluster
            .system_pods
            .iter()
            .map(|pod| {
                vec![
                    pod.name.clone(),
                    pod.phase.clone(),
                    format!("{}/{}", pod.ready_containers, pod.containers),
                ]
            })
            .collect();
        println!();
        print!("{}", format::table(&["KUBE-SYSTEM POD", "PHASE", "READY"], &rows));
    }
}
