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
    /// No container engine is installed for the backend to manage.
    EngineNotInstalled,
}

impl ErrorReason {
    /// Every reason, in `contr.common.v1.ErrorReason` number order.
    pub const ALL: [Self; 7] = [
        Self::EngineNotRunning,
        Self::WslNotInstalled,
        Self::VirtualizationDisabled,
        Self::SocketPermissionDenied,
        Self::SettingLocked,
        Self::FeatureUnavailable,
        Self::EngineNotInstalled,
    ];

    /// Name sent in `google.rpc.ErrorInfo.reason`; clients branch on it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EngineNotRunning => "ENGINE_NOT_RUNNING",
            Self::WslNotInstalled => "WSL_NOT_INSTALLED",
            Self::VirtualizationDisabled => "VIRTUALIZATION_DISABLED",
            Self::SocketPermissionDenied => "SOCKET_PERMISSION_DENIED",
            Self::SettingLocked => "SETTING_LOCKED",
            Self::FeatureUnavailable => "FEATURE_UNAVAILABLE",
            Self::EngineNotInstalled => "ENGINE_NOT_INSTALLED",
        }
    }

    /// One-sentence explanation for users.
    pub const fn description(self) -> &'static str {
        match self {
            Self::EngineNotRunning => "The engine is not running.",
            Self::WslNotInstalled => "WSL 2 is not installed or is turned off.",
            Self::VirtualizationDisabled => "Hardware virtualization is turned off in the firmware.",
            Self::SocketPermissionDenied => "The engine's socket exists, but this user may not open it.",
            Self::SettingLocked => "An administrator policy fixes this setting.",
            Self::FeatureUnavailable => "This backend does not support the request.",
            Self::EngineNotInstalled => "No container engine is installed.",
        }
    }

    /// Parses a name produced by [`Self::as_str`].
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.as_str() == name)
    }
}

impl fmt::Display for ErrorReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Action the daemon can run to resolve a failed precondition; mirrors `contr.common.v1.Remediation`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remediation {
    /// Stable identifier clients send back to run the action; renaming one breaks deployed clients.
    pub action_id: &'static str,
    /// The action shows an operating-system elevation prompt.
    pub requires_elevation: bool,
}

impl Remediation {
    /// Installs the host's container engine packages.
    pub const INSTALL_ENGINE: Self = Self {
        action_id: "install-engine",
        requires_elevation: true,
    };

    /// Adds the user to the `docker` group, which makes the user root-equivalent on the host.
    pub const JOIN_DOCKER_GROUP: Self = Self {
        action_id: "join-docker-group",
        requires_elevation: true,
    };

    /// Sets up a rootless engine for the user.
    pub const SETUP_ROOTLESS: Self = Self {
        action_id: "setup-rootless",
        requires_elevation: false,
    };

    /// Installs WSL 2; Windows may need a restart afterwards.
    pub const INSTALL_WSL: Self = Self {
        action_id: "install-wsl",
        requires_elevation: true,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for reason in ErrorReason::ALL {
            assert_eq!(ErrorReason::from_name(reason.as_str()), Some(reason));
        }
        assert_eq!(ErrorReason::from_name("UNSPECIFIED"), None);
    }
}
