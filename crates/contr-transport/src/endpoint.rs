use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use contr_core::{paths, product};

use crate::{DiscoveryFile, TransportError};

/// Address of the daemon's gRPC API, written `unix:///PATH` or `npipe:////./pipe/NAME` as in `DOCKER_HOST`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Endpoint {
    /// Unix domain socket at an absolute path.
    Unix(PathBuf),
    /// Windows named pipe in `\\.\pipe\NAME` form.
    NamedPipe(String),
}

impl Endpoint {
    /// Endpoint from the `CONTR_HOST` variable, else from the running daemon's discovery file, else the per-user
    /// default.
    pub fn resolve() -> Result<Self, TransportError> {
        if let Some(host) = std::env::var_os(product::HOST_ENV) {
            let host = host
                .into_string()
                .map_err(|host| TransportError::InvalidEndpoint(host.to_string_lossy().into_owned()))?;
            return host.parse();
        }

        let discovered = paths::discovery_file()
            .and_then(|path| DiscoveryFile::read(&path).ok())
            .and_then(|file| file.endpoints.into_iter().find_map(|endpoint| endpoint.parse().ok()));

        discovered.map_or_else(Self::default_for_user, Ok)
    }

    /// Endpoint the daemon serves when started without one.
    pub fn default_for_user() -> Result<Self, TransportError> {
        #[cfg(unix)]
        let endpoint = paths::api_socket().map(Self::Unix).ok_or(TransportError::NoRuntimeDir);
        #[cfg(windows)]
        let endpoint = Ok(Self::NamedPipe(product::api_pipe_name(&crate::current_user_sid()?)));

        endpoint
    }
}

impl FromStr for Endpoint {
    type Err = TransportError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || TransportError::InvalidEndpoint(text.to_owned());

        if let Some(path) = text.strip_prefix("unix://") {
            return path
                .starts_with('/')
                .then(|| Self::Unix(PathBuf::from(path)))
                .ok_or_else(invalid);
        }

        if let Some(path) = text.strip_prefix("npipe://") {
            let name = path.trim_start_matches('/').replace('/', "\\");
            return name
                .strip_prefix(r".\pipe\")
                .filter(|pipe| !pipe.is_empty())
                .map(|_| Self::NamedPipe(format!(r"\\{name}")))
                .ok_or_else(invalid);
        }

        Err(invalid())
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "unix://{}", path.display()),
            Self::NamedPipe(name) => write!(f, "npipe://{}", name.replace('\\', "/")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_unix_sockets() {
        let endpoint: Endpoint = "unix:///run/user/1000/contr/contrd.sock".parse().unwrap();

        assert_eq!(
            endpoint,
            Endpoint::Unix(PathBuf::from("/run/user/1000/contr/contrd.sock"))
        );
        assert_eq!(endpoint.to_string(), "unix:///run/user/1000/contr/contrd.sock");
    }

    #[test]
    fn parses_and_formats_named_pipes() {
        let endpoint: Endpoint = "npipe:////./pipe/contr-S-1-5-21-1-api".parse().unwrap();

        assert_eq!(
            endpoint,
            Endpoint::NamedPipe(r"\\.\pipe\contr-S-1-5-21-1-api".to_owned())
        );
        assert_eq!(endpoint.to_string(), "npipe:////./pipe/contr-S-1-5-21-1-api");
    }

    #[test]
    fn rejects_other_schemes() {
        for text in [
            "tcp://127.0.0.1:2375",
            "unix://relative.sock",
            "npipe:////./other/x",
            "npipe:////./pipe/",
        ] {
            assert!(text.parse::<Endpoint>().is_err(), "{text}");
        }
    }
}
