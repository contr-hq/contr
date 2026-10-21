//! The single-node Kubernetes cluster: k3s running as a container on the engine.

/// Kubernetes versions the cluster runs, newest first; the first is the default.
pub const VERSIONS: [&str; 2] = ["v1.37.1", "v1.36.5"];

/// Name of the cluster's server container.
pub const SERVER_CONTAINER: &str = "contr-k3s-server";

/// Volume holding the cluster's state, which a reset or a version change deletes.
pub const DATA_VOLUME: &str = "contr-k3s";

/// Port the API server listens on, in the container and on the engine's loopback interface.
pub const API_PORT: u16 = 6443;

/// k3s image of `version`, one of [`VERSIONS`].
pub fn image(version: &str) -> String {
    format!("rancher/k3s:{version}-k3s1")
}
