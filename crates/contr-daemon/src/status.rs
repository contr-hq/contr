//! Conversion of daemon failures into gRPC statuses carrying `google.rpc.ErrorInfo`, `Remediation` and
//! `DaemonJsonRejection` details.

use std::collections::HashMap;

use bytes::Bytes;
use contr_api::common::v1 as common;
use contr_api::operations::v1 as operations;
use contr_api::settings::v1 as settings;
use contr_backend::OperationError;
use contr_core::guest::ConfigRejection;
use contr_core::{ErrorReason, Remediation, product};
use contr_engine::EngineError;
use prost::Message;
use prost_types::Any;
use tonic::{Code, Status};
use tonic_types::pb;

const ERROR_INFO_TYPE: &str = "type.googleapis.com/google.rpc.ErrorInfo";
const REMEDIATION_TYPE: &str = "type.googleapis.com/contr.common.v1.Remediation";
const DAEMON_JSON_REJECTION_TYPE: &str = "type.googleapis.com/contr.settings.v1.DaemonJsonRejection";

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

pub(crate) fn setting_locked(path: &str) -> Status {
    with_reason(
        Code::FailedPrecondition,
        &format!("an administrator fixes {path}"),
        ErrorReason::SettingLocked,
        &[],
    )
}

/// Status for an `engine.daemon_json` the engine rejects, naming each key it does not know.
pub(crate) fn daemon_json_rejected(rejection: &ConfigRejection) -> Status {
    let message = format!(
        "engine.daemon_json: the engine rejects its configuration: {}",
        rejection.reason
    );
    let detail = settings::DaemonJsonRejection {
        unknown_keys: rejection
            .unknown_keys
            .iter()
            .map(|unknown| settings::UnknownDaemonKey {
                key: unknown.key.clone(),
                suggestion: unknown.suggestion.clone().unwrap_or_default(),
            })
            .collect(),
    };
    let status = pb::Status {
        code: Code::InvalidArgument.into(),
        message: message.clone(),
        details: vec![Any {
            type_url: DAEMON_JSON_REJECTION_TYPE.to_owned(),
            value: detail.encode_to_vec(),
        }],
    };

    Status::with_details(Code::InvalidArgument, message, Bytes::from(status.encode_to_vec()))
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

/// What a failed operation's `status` says: its code, message, reason and remediations.
pub(crate) fn operation_error(status: &Status) -> operations::OperationError {
    let mut error = operations::OperationError {
        code: status.code().into(),
        message: status.message().to_owned(),
        reason: common::ErrorReason::Unspecified.into(),
        remediations: Vec::new(),
    };
    let Ok(details) = pb::Status::decode(status.details()) else {
        return error;
    };
    for detail in details.details {
        match detail.type_url.as_str() {
            ERROR_INFO_TYPE => {
                if let Ok(info) = pb::ErrorInfo::decode(detail.value.as_slice()) {
                    let reason = common::ErrorReason::from_str_name(&format!("ERROR_REASON_{}", info.reason));
                    error.reason = reason.unwrap_or(common::ErrorReason::Unspecified).into();
                }
            }
            REMEDIATION_TYPE => {
                if let Ok(remediation) = common::Remediation::decode(detail.value.as_slice()) {
                    error.remediations.push(remediation);
                }
            }
            _ => {}
        }
    }

    error
}

/// The status an RPC fails with for a failed operation, with the details [`operation_error`] read.
pub(crate) fn from_operation_error(error: &operations::OperationError) -> Status {
    let code = Code::from(error.code);
    let reason = common::ErrorReason::try_from(error.reason).unwrap_or(common::ErrorReason::Unspecified);
    if reason == common::ErrorReason::Unspecified && error.remediations.is_empty() {
        return Status::new(code, error.message.clone());
    }

    let mut details = Vec::new();
    if reason != common::ErrorReason::Unspecified {
        let error_info = pb::ErrorInfo {
            reason: reason.as_str_name().trim_start_matches("ERROR_REASON_").to_owned(),
            domain: product::ERROR_DOMAIN.to_owned(),
            metadata: HashMap::default(),
        };
        details.push(Any {
            type_url: ERROR_INFO_TYPE.to_owned(),
            value: error_info.encode_to_vec(),
        });
    }
    details.extend(error.remediations.iter().map(|remediation| Any {
        type_url: REMEDIATION_TYPE.to_owned(),
        value: remediation.encode_to_vec(),
    }));
    let status = pb::Status {
        code: error.code,
        message: error.message.clone(),
        details,
    };

    Status::with_details(code, error.message.clone(), Bytes::from(status.encode_to_vec()))
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
    fn operation_errors_keep_the_reason_and_remediations() {
        let remediation = Remediation {
            action_id: "install-wsl",
            requires_elevation: true,
        };
        let status = with_reason(
            Code::FailedPrecondition,
            "WSL is not installed",
            ErrorReason::WslNotInstalled,
            &[remediation],
        );

        let error = operation_error(&status);
        let again = operation_error(&from_operation_error(&error));

        assert_eq!(error.code, i32::from(Code::FailedPrecondition));
        assert_eq!(error.reason(), common::ErrorReason::WslNotInstalled);
        assert_eq!(error.remediations[0].action_id, "install-wsl");
        assert_eq!(again, error);
    }

    #[test]
    fn plain_statuses_become_plain_errors() {
        let error = operation_error(&Status::not_found("no such image"));

        assert_eq!(error.reason(), common::ErrorReason::Unspecified);
        assert_eq!(from_operation_error(&error).code(), Code::NotFound);
    }

    #[test]
    fn every_reason_has_a_proto_value() {
        for reason in ErrorReason::ALL {
            assert_ne!(proto_reason(reason), common::ErrorReason::Unspecified, "{reason}");
        }
    }

    #[test]
    fn daemon_json_rejections_name_the_unknown_keys() {
        let status = daemon_json_rejected(&ConfigRejection {
            reason: "unknown keys".to_owned(),
            unknown_keys: vec![contr_core::guest::UnknownKey {
                key: "log-opt".to_owned(),
                suggestion: Some("log-opts".to_owned()),
            }],
        });

        let details = pb::Status::decode(status.details()).unwrap();
        let rejection = settings::DaemonJsonRejection::decode(details.details[0].value.as_slice()).unwrap();

        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(details.details[0].type_url, DAEMON_JSON_REJECTION_TYPE);
        assert_eq!(rejection.unknown_keys[0].suggestion, "log-opts");
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
