//! Keeps the cluster as the settings ask while the engine runs, and publishes its state.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use contr_api::kubernetes::v1::{Cluster, ClusterState, Step};
use contr_core::{kube, product};
use contr_kube::cluster::{self, Desired};
use contr_kube::kubeconfig;
use tokio::sync::{Notify, watch};

use crate::settings::{SettingsHub, Snapshot};
use crate::slot::{EngineHandle, EngineSlot};

/// How often a ready cluster's pods and the current kubectl context are read again.
const REFRESH: Duration = Duration::from_secs(15);
/// How long a cluster that failed to start waits before the next attempt.
const RETRY: Duration = Duration::from_secs(60);

/// The cluster's published state and the requests that change it.
pub(crate) struct KubernetesHub {
    state: watch::Sender<Cluster>,
    reset: Notify,
    /// Revision of the settings the loop last read.
    revision: AtomicU64,
    /// Kubeconfig holding the cluster's context; `None` when the user has no home directory.
    pub(crate) kubeconfig: Option<PathBuf>,
}

impl KubernetesHub {
    /// Starts keeping the cluster as `settings` ask whenever `slot` holds a running engine.
    pub(crate) fn spawn(slot: &EngineSlot, settings: &SettingsHub, kubeconfig: Option<PathBuf>) -> Arc<Self> {
        let hub = Arc::new(Self {
            state: watch::Sender::new(Cluster::default()),
            reset: Notify::new(),
            revision: AtomicU64::new(0),
            kubeconfig,
        });
        tokio::spawn(keep(Arc::clone(&hub), slot.subscribe(), settings.subscribe()));
        hub
    }

    pub(crate) fn cluster(&self) -> Cluster {
        self.state.borrow().clone()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Cluster> {
        self.state.subscribe()
    }

    /// Deletes the cluster and starts a new one when it is enabled.
    pub(crate) fn request_reset(&self) {
        self.publish(state(ClusterState::Stopping));
        self.reset.notify_one();
    }

    /// Publishes `cluster` with the fields every state shares.
    fn publish(&self, mut cluster: Cluster) {
        cluster.api_server = cluster::api_server();
        product::KUBE_CONTEXT.clone_into(&mut cluster.context);
        cluster.available_versions = kube::VERSIONS.map(str::to_owned).to_vec();
        cluster.settings_etag = self.revision.load(Ordering::Relaxed).to_string();
        if let Some(path) = &self.kubeconfig {
            cluster.kubeconfig = path.display().to_string();
            cluster.current_context = kubeconfig::current_context(path).ok().flatten().unwrap_or_default();
        }
        self.state.send_if_modified(|current| {
            let changed = *current != cluster;
            *current = cluster;
            changed
        });
    }

    /// Publishes the current state again, for a change outside the cluster such as kubectl's current context.
    pub(crate) fn republish(&self) {
        self.publish(self.cluster());
    }
}

/// What the loop last brought about on one engine.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Applied {
    Ready(Desired),
    Failed(Desired),
}

async fn keep(
    hub: Arc<KubernetesHub>,
    mut engine: watch::Receiver<Option<Arc<EngineHandle>>>,
    mut settings: watch::Receiver<Snapshot>,
) {
    let mut applied: Option<Applied> = None;
    let mut engine_id = None;
    let mut reset = false;
    loop {
        let handle = engine.borrow_and_update().clone();
        let id = handle.as_ref().map(|handle| Arc::as_ptr(handle) as usize);
        if id != engine_id {
            engine_id = id;
            applied = None;
        }
        let snapshot = settings.borrow_and_update().clone();
        hub.revision.store(snapshot.revision, Ordering::Relaxed);
        let desired = desired(&snapshot);

        let wait = match (handle, desired) {
            (None, None) => {
                hub.publish(state(ClusterState::Disabled));
                None
            }
            (None, Some(_)) => {
                hub.publish(state(ClusterState::WaitingForEngine));
                None
            }
            (Some(handle), None) => {
                applied = None;
                if std::mem::take(&mut reset) {
                    remove(&hub, &handle).await;
                }
                disable(&hub, &handle).await;
                None
            }
            (Some(handle), Some(desired)) => {
                if std::mem::take(&mut reset) {
                    applied = None;
                    remove(&hub, &handle).await;
                }
                match applied.clone() {
                    Some(Applied::Ready(ready)) if ready == desired => {
                        if refresh(&hub, &handle).await {
                            Some(REFRESH)
                        } else {
                            applied = None;
                            Some(Duration::ZERO)
                        }
                    }
                    Some(Applied::Failed(failed)) if failed == desired => Some(RETRY),
                    _ => {
                        let started = tokio::select! {
                            started = start(&hub, &handle, &desired) => Some(started),
                            changed = settings.changed() => {
                                if changed.is_err() { return; }
                                None
                            }
                            changed = engine.changed() => {
                                if changed.is_err() { return; }
                                None
                            }
                            () = hub.reset.notified() => {
                                reset = true;
                                None
                            }
                        };
                        match started {
                            Some(true) => {
                                applied = Some(Applied::Ready(desired));
                                Some(REFRESH)
                            }
                            Some(false) => {
                                applied = Some(Applied::Failed(desired));
                                Some(RETRY)
                            }
                            None => Some(Duration::ZERO),
                        }
                    }
                }
            }
        };
        if wait == Some(Duration::ZERO) {
            continue;
        }

        let pause = wait.unwrap_or_default();
        tokio::select! {
            changed = engine.changed() => if changed.is_err() { return },
            changed = settings.changed() => if changed.is_err() { return },
            () = hub.reset.notified() => reset = true,
            () = tokio::time::sleep(pause), if wait.is_some() => {
                if matches!(applied, Some(Applied::Failed(_))) {
                    applied = None;
                }
            }
        }
    }
}

/// What the settings ask the cluster to be; `None` while it is disabled.
fn desired(snapshot: &Snapshot) -> Option<Desired> {
    let kubernetes = &snapshot.effective.configured.kubernetes;
    kubernetes.enabled.then(|| Desired {
        version: kubernetes.version().to_owned(),
        traefik: kubernetes.traefik,
    })
}

fn state(state: ClusterState) -> Cluster {
    Cluster {
        state: state.into(),
        ..Cluster::default()
    }
}

/// Starts the cluster, publishing each step; returns whether it became ready.
async fn start(hub: &KubernetesHub, handle: &EngineHandle, desired: &Desired) -> bool {
    let Some(path) = hub.kubeconfig.clone() else {
        hub.publish(Cluster {
            error: "Contr cannot find your home directory, so it has no kubeconfig to add the cluster to".to_owned(),
            ..state(ClusterState::Failed)
        });
        return false;
    };
    let mut steps: Vec<Step> = Vec::new();
    let mut report = |current: &[Step]| {
        steps = current.to_vec();
        hub.publish(Cluster {
            steps: steps.clone(),
            ..state(ClusterState::Starting)
        });
    };

    match cluster::start(&handle.engine, desired, &path, &mut report).await {
        Ok(()) => refresh(hub, handle).await,
        Err(error) => {
            tracing::warn!(error = %error, "the Kubernetes cluster did not start");
            hub.publish(Cluster {
                steps,
                error: error.to_string(),
                ..state(ClusterState::Failed)
            });
            false
        }
    }
}

/// Publishes a running cluster's version and pods; returns false when the server is not running.
async fn refresh(hub: &KubernetesHub, handle: &EngineHandle) -> bool {
    let server = cluster::server(&handle.engine).await.ok().flatten();
    if !server.as_ref().is_some_and(|server| server.running) {
        return false;
    }
    let version = cluster::ready_version(&handle.engine).await.ok().flatten();
    let Some(version) = version else {
        return false;
    };
    let system_pods = cluster::system_pods(&handle.engine).await.unwrap_or_default();

    hub.publish(Cluster {
        version,
        started_at: server.and_then(|server| server.started_at),
        system_pods,
        ..state(ClusterState::Ready)
    });
    true
}

async fn disable(hub: &KubernetesHub, handle: &EngineHandle) {
    let running = cluster::server(&handle.engine)
        .await
        .ok()
        .flatten()
        .is_some_and(|server| server.running);
    if running {
        hub.publish(state(ClusterState::Stopping));
        if let Err(error) = cluster::stop(&handle.engine).await {
            tracing::warn!(error = %error, "cannot stop the Kubernetes cluster");
        }
    }
    hub.publish(state(ClusterState::Disabled));
}

async fn remove(hub: &KubernetesHub, handle: &EngineHandle) {
    hub.publish(state(ClusterState::Stopping));
    if let Err(error) = cluster::remove(&handle.engine).await {
        tracing::warn!(error = %error, "cannot delete the Kubernetes cluster");
    }
}
