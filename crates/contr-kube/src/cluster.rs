//! The cluster's server container on the engine: starting it as the settings ask, its state, and images copied into it.

use std::path::Path;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use contr_api::containers::v1::ContainerState;
use contr_api::kubernetes::v1::{Pod, Step, StepKind, StepStatus};
use contr_core::{kube, product};
use contr_engine::{Engine, EngineError, RunOutput, SystemContainer};
use futures_util::{Stream, StreamExt, stream};
use prost_types::Timestamp;
use serde_json::Value;

use crate::kubeconfig::{self, Access, KubeconfigError};

/// Labels recording what the server container was created for.
const VERSION_LABEL: &str = "io.github.contr_hq.contr.kubernetes.version";
const TRAEFIK_LABEL: &str = "io.github.contr_hq.contr.kubernetes.traefik";
/// Where k3s keeps the cluster's state, on the data volume.
const DATA_DIR: &str = "/var/lib/rancher/k3s";
/// Kubeconfig k3s writes for its own API server.
const K3S_KUBECONFIG: &str = "/etc/rancher/k3s/k3s.yaml";
/// A first start pulls nothing more but still sets up the node; later starts take seconds.
const NODE_WAIT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// What the settings ask the cluster to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Desired {
    /// One of [`kube::VERSIONS`].
    pub version: String,
    pub traefik: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Kubeconfig(#[from] KubeconfigError),
    #[error("{0}")]
    Failed(String),
}

/// URL kubectl reaches the API server at.
pub fn api_server() -> String {
    format!("https://127.0.0.1:{}", kube::API_PORT)
}

/// Runs the server container as `desired` asks, waits for its node, and adds its context to the kubeconfig at
/// `kubeconfig_path`, reporting the steps through `report` whenever one changes. A different version starts a new
/// cluster; a different Traefik choice keeps the cluster's data.
pub async fn start(
    engine: &Engine,
    desired: &Desired,
    kubeconfig_path: &Path,
    report: &mut (dyn FnMut(&[Step]) + Send),
) -> Result<(), ClusterError> {
    let mut steps = Steps::new();
    let image = kube::image(&desired.version);

    steps.begin(StepKind::PullImage, &image, report);
    let pulled = pull(engine, &image).await;
    steps.end(StepKind::PullImage, pulled, report)?;

    steps.begin(StepKind::StartServer, "", report);
    let started = run_server(engine, desired, &image)
        .await
        .map(|()| format!("127.0.0.1:{}", kube::API_PORT));
    steps.end(StepKind::StartServer, started, report)?;

    steps.begin(StepKind::WaitForNode, "", report);
    let ready = wait_for_node(engine).await;
    steps.end(StepKind::WaitForNode, ready, report)?;

    steps.begin(StepKind::AddKubeconfig, &kubeconfig_path.display().to_string(), report);
    let added = add_context(engine, kubeconfig_path)
        .await
        .map(|()| product::KUBE_CONTEXT.to_owned());
    steps.end(StepKind::AddKubeconfig, added, report)?;

    Ok(())
}

/// Stops the server container, keeping the cluster's data.
pub async fn stop(engine: &Engine) -> Result<(), ClusterError> {
    if server(engine).await?.is_some_and(|server| server.running) {
        engine.stop_container(kube::SERVER_CONTAINER, None).await?;
    }
    Ok(())
}

/// Deletes the server container and the cluster's data.
pub async fn remove(engine: &Engine) -> Result<(), ClusterError> {
    if server(engine).await?.is_some() {
        engine.remove_container(kube::SERVER_CONTAINER, true, false).await?;
    }
    match engine.remove_volume(kube::DATA_VOLUME, true).await {
        Ok(()) | Err(EngineError::NotFound(_)) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// The server container as the engine reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Server {
    pub running: bool,
    pub started_at: Option<Timestamp>,
    pub desired: Option<Desired>,
}

/// The server container, or `None` when there is none.
pub async fn server(engine: &Engine) -> Result<Option<Server>, ClusterError> {
    let inspected = match engine.inspect_container(kube::SERVER_CONTAINER).await {
        Ok(inspected) => inspected,
        Err(EngineError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let container = inspected.container.unwrap_or_default();
    let labels = &container.labels;
    let desired = labels.get(VERSION_LABEL).map(|version| Desired {
        version: version.clone(),
        traefik: labels.get(TRAEFIK_LABEL).is_some_and(|traefik| traefik == "true"),
    });

    Ok(Some(Server {
        running: container.state() == ContainerState::Running,
        started_at: inspected.details.and_then(|details| details.started_at),
        desired,
    }))
}

/// The node's k3s version once it is ready; `None` while it is not.
pub async fn ready_version(engine: &Engine) -> Result<Option<String>, ClusterError> {
    let nodes = kubectl(engine, &["get", "nodes", "-o", "json"]).await?;
    let node = &nodes["items"][0];
    let ready = node["status"]["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|condition| condition["type"] == "Ready" && condition["status"] == "True");

    Ok(ready.then(|| {
        node["status"]["nodeInfo"]["kubeletVersion"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }))
}

/// Pods of the kube-system namespace, sorted by name.
pub async fn system_pods(engine: &Engine) -> Result<Vec<Pod>, ClusterError> {
    let pods = kubectl(engine, &["get", "pods", "--namespace", "kube-system", "-o", "json"]).await?;
    let mut pods: Vec<Pod> = pods["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|pod| {
            let count = |value: &Value| u32::try_from(value.as_array().map_or(0, Vec::len)).unwrap_or(u32::MAX);
            let ready = pod["status"]["containerStatuses"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|status| status["ready"] == true)
                .count();
            Pod {
                name: pod["metadata"]["name"].as_str().unwrap_or_default().to_owned(),
                phase: pod["status"]["phase"].as_str().unwrap_or_default().to_owned(),
                ready_containers: u32::try_from(ready).unwrap_or(u32::MAX),
                containers: count(&pod["spec"]["containers"]),
            }
        })
        .collect();
    pods.sort_by(|left, right| left.name.cmp(&right.name));

    Ok(pods)
}

/// Copies `image` from the engine into the cluster's image store.
pub async fn import(engine: &Engine, image: &str) -> Result<(), ClusterError> {
    match engine.inspect_image(image).await {
        Ok(_) => {}
        Err(EngineError::NotFound(_)) => {
            return Err(ClusterError::Failed(format!("the engine has no image {image}")));
        }
        Err(error) => return Err(error.into()),
    }
    let archive = engine.export_images(&[image.to_owned()]);
    let output = engine
        .run_in(
            kube::SERVER_CONTAINER,
            &["ctr", "--namespace", "k8s.io", "images", "import", "-"],
            Some(archive.boxed()),
        )
        .await?;
    succeeded(&output, "import the image")
}

async fn pull(engine: &Engine, image: &str) -> Result<String, ClusterError> {
    let present = match engine.inspect_image(image).await {
        Ok(_) => true,
        Err(EngineError::NotFound(_)) => false,
        Err(error) => return Err(error.into()),
    };
    if !present {
        let mut progress = engine.pull_image(image, "").boxed();
        while let Some(event) = progress.next().await {
            event?;
        }
    }
    let size = engine
        .inspect_image(image)
        .await?
        .image
        .map_or(0, |image| image.size_bytes);

    Ok(format!("{} MB", size / 1_000_000))
}

async fn run_server(engine: &Engine, desired: &Desired, image: &str) -> Result<(), ClusterError> {
    match server(engine).await? {
        Some(existing) if existing.desired.as_ref() == Some(desired) => {
            if !existing.running {
                engine.start_container(kube::SERVER_CONTAINER).await?;
            }
            return Ok(());
        }
        Some(existing) => {
            engine.remove_container(kube::SERVER_CONTAINER, true, false).await?;
            let same_version = existing
                .desired
                .is_some_and(|created| created.version == desired.version);
            if !same_version {
                remove(engine).await?;
            }
        }
        None => {}
    }

    let mut command = vec![
        "server".to_owned(),
        "--tls-san".to_owned(),
        "127.0.0.1".to_owned(),
        "--https-listen-port".to_owned(),
        kube::API_PORT.to_string(),
    ];
    if !desired.traefik {
        command.extend(["--disable".to_owned(), "traefik".to_owned()]);
    }
    let container = SystemContainer {
        name: kube::SERVER_CONTAINER.to_owned(),
        image: image.to_owned(),
        hostname: product::KUBE_CONTEXT.to_owned(),
        command,
        volumes: vec![(kube::DATA_VOLUME.to_owned(), DATA_DIR.to_owned())],
        tmpfs: vec!["/run".to_owned(), "/var/run".to_owned()],
        loopback_port: Some(kube::API_PORT),
        labels: [
            (VERSION_LABEL.to_owned(), desired.version.clone()),
            (TRAEFIK_LABEL.to_owned(), desired.traefik.to_string()),
        ]
        .into(),
    };
    engine.create_system_container(&container).await?;
    engine.start_container(kube::SERVER_CONTAINER).await?;

    Ok(())
}

async fn wait_for_node(engine: &Engine) -> Result<String, ClusterError> {
    let started = SystemTime::now();
    loop {
        // The API server refuses connections for a few seconds after the container starts.
        if let Ok(Some(_)) = ready_version(engine).await {
            let waited = started.elapsed().unwrap_or_default().as_secs();
            return Ok(format!("{waited} s"));
        }
        if started.elapsed().unwrap_or_default() > NODE_WAIT {
            return Err(ClusterError::Failed(format!(
                "the node was not ready after {} minutes",
                NODE_WAIT.as_secs() / 60
            )));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn add_context(engine: &Engine, kubeconfig_path: &Path) -> Result<(), ClusterError> {
    let output = engine
        .run_in(kube::SERVER_CONTAINER, &["cat", K3S_KUBECONFIG], no_input())
        .await?;
    succeeded(&output, "read the cluster's kubeconfig")?;
    let access = Access::from_k3s(&String::from_utf8_lossy(&output.stdout), &api_server())?;
    kubeconfig::merge(kubeconfig_path, product::KUBE_CONTEXT, &access)?;

    Ok(())
}

async fn kubectl(engine: &Engine, args: &[&str]) -> Result<Value, ClusterError> {
    let command: Vec<&str> = ["kubectl"].into_iter().chain(args.iter().copied()).collect();
    let output = engine.run_in(kube::SERVER_CONTAINER, &command, no_input()).await?;
    succeeded(&output, "ask the cluster")?;

    serde_json::from_slice(&output.stdout)
        .map_err(|error| ClusterError::Failed(format!("kubectl printed no JSON: {error}")))
}

fn succeeded(output: &RunOutput, action: &str) -> Result<(), ClusterError> {
    if output.exit_code == 0 {
        Ok(())
    } else {
        Err(ClusterError::Failed(format!("cannot {action}: {}", output.error())))
    }
}

fn no_input() -> Option<impl Stream<Item = Result<Bytes, EngineError>> + Send + Unpin> {
    None::<stream::Empty<Result<Bytes, EngineError>>>
}

/// The steps of starting, in order.
struct Steps(Vec<Step>);

impl Steps {
    fn new() -> Self {
        Self(
            [
                StepKind::PullImage,
                StepKind::StartServer,
                StepKind::WaitForNode,
                StepKind::AddKubeconfig,
            ]
            .into_iter()
            .map(|kind| Step {
                kind: kind.into(),
                status: StepStatus::Pending.into(),
                ..Step::default()
            })
            .collect(),
        )
    }

    fn step(&mut self, kind: StepKind) -> Option<&mut Step> {
        self.0.iter_mut().find(|step| step.kind() == kind)
    }

    fn begin(&mut self, kind: StepKind, detail: &str, report: &mut (dyn FnMut(&[Step]) + Send)) {
        if let Some(step) = self.step(kind) {
            step.set_status(StepStatus::Running);
            detail.clone_into(&mut step.detail);
            step.started_at = Some(SystemTime::now().into());
        }
        report(&self.0);
    }

    /// Ends `kind` with `outcome`, whose success is the step's detail, and passes a failure on.
    fn end(
        &mut self,
        kind: StepKind,
        outcome: Result<String, ClusterError>,
        report: &mut (dyn FnMut(&[Step]) + Send),
    ) -> Result<(), ClusterError> {
        if let Some(step) = self.step(kind) {
            step.finished_at = Some(SystemTime::now().into());
            match &outcome {
                Ok(detail) => {
                    step.set_status(StepStatus::Done);
                    step.detail.clone_from(detail);
                }
                Err(error) => {
                    step.set_status(StepStatus::Failed);
                    step.detail = error.to_string();
                }
            }
        }
        report(&self.0);

        outcome.map(drop)
    }
}
