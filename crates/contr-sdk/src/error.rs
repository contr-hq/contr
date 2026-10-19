use contr_api::common::v1::Remediation;
use contr_core::ErrorReason;
use contr_transport::{Endpoint, TransportError};
use prost::Message;
use tonic::{Code, Status};
use tonic_types::pb;

const ERROR_INFO_TYPE: &str = "type.googleapis.com/google.rpc.ErrorInfo";
const REMEDIATION_TYPE: &str = "type.googleapis.com/contr.common.v1.Remediation";

/// Failure of an SDK call, classified from the gRPC status and its `google.rpc.ErrorInfo` reason.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// No daemon answered at the endpoint.
    #[error("cannot reach the daemon at {endpoint}: {source}")]
    Connect {
        /// Endpoint that was tried.
        endpoint: String,
        /// Transport failure.
        #[source]
        source: TransportError,
    },
    /// The endpoint could not be determined.
    #[error(transparent)]
    Endpoint(TransportError),
    /// The daemon did not come up after being started.
    #[error("the daemon did not start: {0}")]
    DaemonStart(String),
    /// The engine is stopped or still starting.
    #[error("{0}")]
    EngineNotRunning(String),
    /// The object does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The object already exists, or its state forbids the request.
    #[error("{0}")]
    Conflict(String),
    /// The daemon or its backend does not support the request.
    #[error("{0}")]
    FeatureUnavailable(String),
    /// Access was refused.
    #[error("{message}")]
    PermissionDenied {
        /// Daemon message.
        message: String,
        /// Actions that grant access, in the order to offer them.
        remediations: Vec<Remediation>,
    },
    /// An administrator policy fixes the setting.
    #[error("{0}")]
    SettingLocked(String),
    /// The daemon does not speak the Contr API this SDK expects.
    #[error("{0}")]
    VersionMismatch(String),
    /// Another unmet precondition the daemon can name and possibly resolve.
    #[error("{message}")]
    Blocked {
        /// Machine-readable cause.
        reason: ErrorReason,
        /// Daemon message.
        message: String,
        /// Actions that resolve the failure, in the order to offer them.
        remediations: Vec<Remediation>,
    },
    /// The call ended before the operation could be sent.
    #[error("the call has ended")]
    Closed,
    /// The daemon broke the protocol.
    #[error("unexpected response: {0}")]
    Protocol(&'static str),
    /// Any other gRPC failure.
    #[error(transparent)]
    Rpc(Status),
}

impl Error {
    pub(crate) fn connect(endpoint: &Endpoint, source: TransportError) -> Self {
        Self::Connect {
            endpoint: endpoint.to_string(),
            source,
        }
    }

    /// Reason the daemon attached, if any.
    pub fn reason(&self) -> Option<ErrorReason> {
        match self {
            Self::EngineNotRunning(_) => Some(ErrorReason::EngineNotRunning),
            Self::FeatureUnavailable(_) => Some(ErrorReason::FeatureUnavailable),
            Self::SettingLocked(_) => Some(ErrorReason::SettingLocked),
            Self::Blocked { reason, .. } => Some(*reason),
            _ => None,
        }
    }
}

impl From<Status> for Error {
    fn from(status: Status) -> Self {
        let (reason, remediations) = details(&status);
        let message = status.message().to_owned();

        match (reason, status.code()) {
            (Some(ErrorReason::EngineNotRunning), _) => Self::EngineNotRunning(message),
            (Some(ErrorReason::FeatureUnavailable), _) | (None, Code::Unimplemented) => {
                Self::FeatureUnavailable(message)
            }
            (Some(ErrorReason::SettingLocked), _) => Self::SettingLocked(message),
            (Some(ErrorReason::SocketPermissionDenied), _) | (None, Code::PermissionDenied) => {
                Self::PermissionDenied { message, remediations }
            }
            (Some(reason), _) => Self::Blocked {
                reason,
                message,
                remediations,
            },
            (None, Code::NotFound) => Self::NotFound(message),
            (None, Code::FailedPrecondition | Code::AlreadyExists | Code::Aborted) => Self::Conflict(message),
            (None, _) => Self::Rpc(status),
        }
    }
}

fn details(status: &Status) -> (Option<ErrorReason>, Vec<Remediation>) {
    let Ok(rich) = pb::Status::decode(status.details()) else {
        return (None, Vec::new());
    };

    let mut reason = None;
    let mut remediations = Vec::new();
    for detail in rich.details {
        match detail.type_url.as_str() {
            ERROR_INFO_TYPE => {
                reason = pb::ErrorInfo::decode(detail.value.as_slice())
                    .ok()
                    .and_then(|info| ErrorReason::from_name(&info.reason));
            }
            REMEDIATION_TYPE => remediations.extend(Remediation::decode(detail.value.as_slice()).ok()),
            _ => {}
        }
    }

    (reason, remediations)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use prost_types::Any;

    use super::*;

    fn status(code: Code, reason: Option<&str>) -> Status {
        let details = reason
            .map(|reason| Any {
                type_url: ERROR_INFO_TYPE.to_owned(),
                value: pb::ErrorInfo {
                    reason: reason.to_owned(),
                    ..Default::default()
                }
                .encode_to_vec(),
            })
            .into_iter()
            .collect();
        let rich = pb::Status {
            code: code.into(),
            message: "message".to_owned(),
            details,
        };
        Status::with_details(code, "message", Bytes::from(rich.encode_to_vec()))
    }

    #[test]
    fn classifies_by_reason_before_code() {
        assert!(matches!(
            Error::from(status(Code::FailedPrecondition, Some("ENGINE_NOT_RUNNING"))),
            Error::EngineNotRunning(_)
        ));
        assert!(matches!(
            Error::from(status(Code::FailedPrecondition, Some("ENGINE_NOT_INSTALLED"))),
            Error::Blocked {
                reason: ErrorReason::EngineNotInstalled,
                ..
            }
        ));
    }

    #[test]
    fn classifies_plain_codes() {
        assert!(matches!(Error::from(Status::not_found("x")), Error::NotFound(_)));
        assert!(matches!(
            Error::from(Status::failed_precondition("x")),
            Error::Conflict(_)
        ));
        assert!(matches!(
            Error::from(Status::unimplemented("x")),
            Error::FeatureUnavailable(_)
        ));
        assert!(matches!(Error::from(Status::internal("x")), Error::Rpc(_)));
    }
}
