use std::path::{Path, PathBuf};

/// Engine the host backend can manage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    DockerRootless,
    DockerRootful,
    PodmanRootless,
    PodmanRootful,
    /// A socket the user named; Contr neither starts nor stops its engine.
    External,
}

/// systemd instance that owns an engine's units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bus {
    System,
    /// The user's own systemd instance.
    Session,
}

/// Where a host engine listens and which systemd units run it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub flavor: Flavor,
    pub socket: PathBuf,
    pub bus: Bus,
    pub start_unit: Option<&'static str>,
    /// Socket units come before services, so socket activation cannot restart a stopped service.
    pub stop_units: &'static [&'static str],
}

/// Candidates in discovery order: an explicit socket alone, else rootless Docker, rootful Docker, then Podman.
pub fn candidates(runtime_dir: Option<&Path>, explicit_socket: Option<&Path>) -> Vec<Candidate> {
    if let Some(socket) = explicit_socket {
        return vec![Candidate {
            flavor: Flavor::External,
            socket: socket.to_path_buf(),
            bus: Bus::System,
            start_unit: None,
            stop_units: &[],
        }];
    }

    let user = |flavor, socket: &str, start_unit, stop_units| {
        runtime_dir.map(|dir| Candidate {
            flavor,
            socket: dir.join(socket),
            bus: Bus::Session,
            start_unit: Some(start_unit),
            stop_units,
        })
    };
    let system = |flavor, socket: &str, start_unit, stop_units| {
        Some(Candidate {
            flavor,
            socket: PathBuf::from(socket),
            bus: Bus::System,
            start_unit: Some(start_unit),
            stop_units,
        })
    };

    [
        user(
            Flavor::DockerRootless,
            "docker.sock",
            "docker.service",
            &["docker.service"],
        ),
        system(
            Flavor::DockerRootful,
            "/var/run/docker.sock",
            "docker.service",
            &["docker.socket", "docker.service"],
        ),
        user(
            Flavor::PodmanRootless,
            "podman/podman.sock",
            "podman.socket",
            &["podman.socket", "podman.service"],
        ),
        system(
            Flavor::PodmanRootful,
            "/run/podman/podman.sock",
            "podman.socket",
            &["podman.socket", "podman.service"],
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_rootless_then_rootful_then_podman() {
        let flavors: Vec<_> = candidates(Some(Path::new("/run/user/1000")), None)
            .into_iter()
            .map(|candidate| (candidate.flavor, candidate.socket))
            .collect();

        assert_eq!(
            flavors,
            [
                (Flavor::DockerRootless, PathBuf::from("/run/user/1000/docker.sock")),
                (Flavor::DockerRootful, PathBuf::from("/var/run/docker.sock")),
                (
                    Flavor::PodmanRootless,
                    PathBuf::from("/run/user/1000/podman/podman.sock")
                ),
                (Flavor::PodmanRootful, PathBuf::from("/run/podman/podman.sock")),
            ]
        );
    }

    #[test]
    fn skips_user_engines_without_a_runtime_directory() {
        let flavors: Vec<_> = candidates(None, None)
            .into_iter()
            .map(|candidate| candidate.flavor)
            .collect();

        assert_eq!(flavors, [Flavor::DockerRootful, Flavor::PodmanRootful]);
    }

    #[test]
    fn an_explicit_socket_is_the_only_candidate() {
        let explicit = Path::new("/tmp/engine.sock");
        let found = candidates(Some(Path::new("/run/user/1000")), Some(explicit));

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].flavor, Flavor::External);
        assert!(found[0].start_unit.is_none());
    }
}
