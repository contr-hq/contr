//! The single-node Kubernetes cluster: k3s running as a container on the engine.

/// Kubernetes versions the cluster runs, newest first, with the digest of each one's k3s image index. The server runs
/// privileged, so it runs only an image whose digest is pinned here, never whatever a tag points at.
const IMAGES: [(&str, &str); 2] = [
    (
        "v1.37.1",
        "sha256:ca7f37d993d82ef0dcdcfecb2e0e2618ea541dbaffc620c8cedebe01a82acd0d",
    ),
    (
        "v1.36.5",
        "sha256:057cfd5a9d51b89f1f0ee139c28d5d5fdd5aa7ddeac2742a0ba90c6dbc0329bb",
    ),
];

/// Kubernetes versions the cluster runs, newest first; the first is the default.
pub const VERSIONS: [&str; 2] = [IMAGES[0].0, IMAGES[1].0];

/// Name of the cluster's server container.
pub const SERVER_CONTAINER: &str = "contr-k3s-server";

/// Volume holding the cluster's state, which a reset or a version change deletes.
pub const DATA_VOLUME: &str = "contr-k3s";

/// Port the API server listens on, in the container and on the engine's loopback interface.
pub const API_PORT: u16 = 6443;

/// Pinned k3s image of `version`; `None` for a version not in [`VERSIONS`].
pub fn image(version: &str) -> Option<String> {
    IMAGES
        .iter()
        .find(|(known, _)| *known == version)
        .map(|(_, digest)| format!("rancher/k3s:{version}-k3s1@{digest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_version_runs_a_pinned_image() {
        for version in VERSIONS {
            let image = image(version).unwrap();
            assert!(
                image.starts_with(&format!("rancher/k3s:{version}-k3s1@sha256:")),
                "{image}"
            );
            assert_eq!(image.len(), format!("rancher/k3s:{version}-k3s1@sha256:").len() + 64);
        }
        assert_eq!(image("v1.20.0"), None);
    }
}
