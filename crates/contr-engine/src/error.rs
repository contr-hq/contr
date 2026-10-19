use bollard::errors::Error as DockerError;

/// Failure of an engine API call, classified the way the daemon maps it to gRPC codes.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The object does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The object already exists, or its state forbids the request.
    #[error("{0}")]
    Conflict(String),
    /// The engine rejected the request's arguments.
    #[error("{0}")]
    InvalidArgument(String),
    /// The engine refused access.
    #[error("{0}")]
    PermissionDenied(String),
    /// The engine did not answer.
    #[error("the engine is unreachable: {0}")]
    Unreachable(#[source] DockerError),
    /// Any other engine failure.
    #[error("{0}")]
    Engine(String),
}

impl From<DockerError> for EngineError {
    fn from(error: DockerError) -> Self {
        match error {
            DockerError::DockerResponseServerError { status_code, message } => match status_code {
                400 => Self::InvalidArgument(message),
                401 | 403 => Self::PermissionDenied(message),
                404 => Self::NotFound(message),
                409 => Self::Conflict(message),
                _ => Self::Engine(message),
            },
            DockerError::DockerStreamError { error } => Self::Engine(error),
            error @ (DockerError::IOError { .. }
            | DockerError::HyperResponseError { .. }
            | DockerError::HyperLegacyError { .. }
            | DockerError::RequestTimeoutError
            | DockerError::SocketNotFoundError(_)) => Self::Unreachable(error),
            other => Self::Engine(other.to_string()),
        }
    }
}
