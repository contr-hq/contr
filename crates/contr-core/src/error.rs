use std::fmt;

/// Machine-readable failure reason; mirrors `contr.common.v1.ErrorReason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorReason {
    /// The engine is stopped or still starting.
    EngineNotRunning,
    /// WSL 2 is missing or disabled by policy.
    WslNotInstalled,
    /// Hardware virtualization is disabled in firmware.
    VirtualizationDisabled,
    /// The engine socket exists but the user may not open it.
    SocketPermissionDenied,
    /// An administrator policy fixes the setting.
    SettingLocked,
    /// The daemon or its backend does not support the request.
    FeatureUnavailable,
}

impl ErrorReason {
    /// Name sent in `google.rpc.ErrorInfo.reason`; clients branch on it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EngineNotRunning => "ENGINE_NOT_RUNNING",
            Self::WslNotInstalled => "WSL_NOT_INSTALLED",
            Self::VirtualizationDisabled => "VIRTUALIZATION_DISABLED",
            Self::SocketPermissionDenied => "SOCKET_PERMISSION_DENIED",
            Self::SettingLocked => "SETTING_LOCKED",
            Self::FeatureUnavailable => "FEATURE_UNAVAILABLE",
        }
    }
}

impl fmt::Display for ErrorReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Action the daemon can run to resolve a failed precondition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remediation {
    /// Stable identifier clients send back to run the action.
    pub action_id: &'static str,
    /// The action shows an operating-system elevation prompt.
    pub requires_elevation: bool,
}
