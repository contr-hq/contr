//! Conversion of daemon failures into gRPC statuses carrying `google.rpc.ErrorInfo` and `Remediation` details.

use std::collections::HashMap;

use bytes::Bytes;
use contr_api::common::v1 as common;
use contr_backend::OperationError;
use contr_core::{ErrorReason, Remediation, product};
use contr_engine::EngineError;
use prost::Message;
use prost_types::Any;
use tonic::{Code, Status};
use tonic_types::pb;

const ERROR_INFO_TYPE: &str = "type.googleapis.com/google.rpc.ErrorInfo";
const REMEDIATION_TYPE: &str = "type.googleapis.com/contr.common.v1.Remediation";

pub(crate) fn engine_not_running() -> Status {
    with_reason(
        Code::FailedPrecondition,
        "the engine is not running",
        ErrorReason::EngineNotRunning,
        &[],
    )
}

pub(crate) fn feature_unavailable(message: &str) -> Status {
    with_reason(Code::FailedPrecondition, message, ErrorReason::FeatureUnavailable, &[])
}

pub(crate) fn from_engine(error: EngineError) -> Status {
    match error {
        EngineError::NotFound(message) => Status::not_found(message),
        EngineError::Conflict(message) => Status::failed_precondition(message),
        EngineError::InvalidArgument(message) => Status::invalid_argument(message),
        EngineError::PermissionDenied(message) => Status::permission_denied(message),
        error @ EngineError::Unreachable(_) => with_reason(
            Code::Unavailable,
            &error.to_string(),
            ErrorReason::EngineNotRunning,
            &[],
        ),
        EngineError::Engine(message) => Status::unknown(message),
    }
}

pub(crate) fn from_operation(error: &OperationError) -> Status {
    let code = match error.reason {
        ErrorReason::SocketPermissionDenied => Code::PermissionDenied,
        _ => Code::FailedPrecondition,
    };
    with_reason(code, &error.message, error.reason, &error.remediations)
}

fn with_reason(code: Code, message: &str, reason: ErrorReason, remediations: &[Remediation]) -> Status {
    let error_info = pb::ErrorInfo {
        reason: reason.as_str().to_owned(),
        domain: product::ERROR_DOMAIN.to_owned(),
        metadata: HashMap::default(),
    };
    let mut details = vec![Any {
        type_url: ERROR_INFO_TYPE.to_owned(),
        value: error_info.encode_to_vec(),
    }];
    details.extend(remediations.iter().map(|remediation| {
        Any {
            type_url: REMEDIATION_TYPE.to_owned(),
            value: common::Remediation {
                action_id: remediation.action_id.to_owned(),
                requires_elevation: remediation.requires_elevation,
            }
            .encode_to_vec(),
        }
    }));

    let status = pb::Status {
        code: code.into(),
        message: message.to_owned(),
        details,
    };
    Status::with_details(code, message, Bytes::from(status.encode_to_vec()))
}

/// `contr.common.v1.ErrorReason` value of `reason`.
pub(crate) fn proto_reason(reason: ErrorReason) -> common::ErrorReason {
    common::ErrorReason::from_str_name(&format!("ERROR_REASON_{}", reason.as_str()))
        .unwrap_or(common::ErrorReason::Unspecified)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_has_a_proto_value() {
        for reason in ErrorReason::ALL {
            assert_ne!(proto_reason(reason), common::ErrorReason::Unspecified, "{reason}");
        }
    }

    #[test]
    fn statuses_carry_the_reason_and_remediations() {
        let status = from_operation(&OperationError {
            reason: ErrorReason::SocketPermissionDenied,
            message: "denied".to_owned(),
            remediations: vec![Remediation::SETUP_ROOTLESS],
        });

        let details = pb::Status::decode(status.details()).unwrap();
        let info = pb::ErrorInfo::decode(details.details[0].value.as_slice()).unwrap();
        let remediation = common::Remediation::decode(details.details[1].value.as_slice()).unwrap();

        assert_eq!(status.code(), Code::PermissionDenied);
        assert_eq!(info.reason, "SOCKET_PERMISSION_DENIED");
        assert_eq!(remediation.action_id, "setup-rootless");
    }
}
