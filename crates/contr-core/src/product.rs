//! Every identifier derived from the product name; renaming the product is a change to this module only.

/// Name shown to users.
pub const DISPLAY_NAME: &str = "Contr";

/// Lowercase stem of the derived identifiers below.
pub const SLUG: &str = "contr";

/// Reverse-DNS application ID used for desktop entries, D-Bus and Flatpak.
pub const APP_ID: &str = "io.github.contr_hq.Contr";

/// Executable name of the daemon.
pub const DAEMON_BINARY: &str = "contrd";

/// Executable name of the command-line interface.
pub const CLI_BINARY: &str = "contr";

/// Executable name of the guest agent.
pub const AGENT_BINARY: &str = "contr-agent";

/// Executable name of the privileged Linux helper.
pub const HELPER_BINARY: &str = "contr-helper";

/// Docker CLI context that points at the engine Contr manages.
pub const DOCKER_CONTEXT: &str = "contr";

/// kubeconfig context of the Contr Kubernetes cluster.
pub const KUBE_CONTEXT: &str = "contr";

/// WSL distribution that runs the engine; replaced on every runtime upgrade.
pub const WSL_RUNTIME_DISTRO: &str = "contr";

/// WSL distribution that holds engine data; never replaced by upgrades.
pub const WSL_DATA_DISTRO: &str = "contr-data";

/// Environment variable that overrides daemon endpoint discovery.
pub const HOST_ENV: &str = "CONTR_HOST";

/// Environment variable holding the daemon's log filter, in `tracing-subscriber` `EnvFilter` syntax.
pub const LOG_ENV: &str = "CONTR_LOG";

/// File name of the daemon's gRPC socket inside the user's runtime directory.
pub const API_SOCKET_FILE: &str = "contrd.sock";

/// File in the user's runtime directory where the running daemon publishes its endpoints.
pub const DISCOVERY_FILE: &str = "daemon.json";

/// systemd user unit that runs the daemon.
pub const DAEMON_UNIT: &str = "contrd.service";

/// Log file in the user's runtime directory for a daemon an SDK started without systemd.
pub const DAEMON_LOG_FILE: &str = "contrd.log";

/// `google.rpc.ErrorInfo.domain` of every error the daemon reports.
pub const ERROR_DOMAIN: &str = "io.github.contr_hq.contr";

/// polkit action that authorizes engine installation and service management.
pub const POLKIT_MANAGE_ENGINE: &str = "io.github.contr_hq.contr.manage-engine";

/// Named pipe of the gRPC API; the SID keeps users of one Windows host apart.
pub fn api_pipe_name(user_sid: &str) -> String {
    format!(r"\\.\pipe\{SLUG}-{user_sid}-api")
}

/// Named pipe of the Docker Engine API proxy; the SID keeps users of one Windows host apart.
pub fn engine_pipe_name(user_sid: &str) -> String {
    format!(r"\\.\pipe\{SLUG}-{user_sid}-engine")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_names_are_scoped_to_the_user() {
        let sid = "S-1-5-21-1004336348-1177238915-682003330-1001";

        assert_eq!(api_pipe_name(sid), format!(r"\\.\pipe\contr-{sid}-api"));
        assert_eq!(engine_pipe_name(sid), format!(r"\\.\pipe\contr-{sid}-engine"));
    }
}
