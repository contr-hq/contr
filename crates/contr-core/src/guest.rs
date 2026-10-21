//! Layout of the Contr guest (WSL distributions and virtual machines) that `contrd` and `contr-agent` both rely on.
//! Both binaries ship in one release, so these values change only together with the guest image.

use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::proxy::Proxy;

/// Absolute path of the agent inside every guest image.
pub const AGENT_PATH: &str = "/usr/local/bin/contr-agent";

/// File holding the release the guest image was built from; `contrd` replaces a runtime whose release differs.
pub const RELEASE_FILE: &str = "/etc/contr-release";

/// Docker Engine API socket inside the runtime guest.
pub const ENGINE_SOCKET: &str = "/var/run/docker.sock";

/// Directory inside the data guest that holds engine data.
pub const DATA_SOURCE: &str = "/var/lib/contr-data";

/// Guest address that `host.docker.internal` resolves to; the agent relays its TCP connections to the host's
/// loopback interface. It lies in 198.18.0.0/15, reserved for benchmarking, so no real network uses it.
pub const HOST_ADDRESS: Ipv4Addr = Ipv4Addr::new(198, 19, 255, 254);

/// Directory under which WSL mounts each Windows drive in the runtime guest, as `/mnt/c`.
pub const WINDOWS_DRIVES: &str = "/mnt";

/// Mount point of the data guest's storage, shared by every distribution of the WSL 2 virtual machine.
pub const DATA_MOUNT: &str = "/mnt/wsl/contr/data";

/// Line the data anchor writes to standard output once [`DATA_MOUNT`] is in place.
pub const DATA_READY: &str = "contr-data ready";

/// Second engine socket, in the directory every distribution of the WSL 2 virtual machine shares; the user's
/// integrated distributions reach the engine through it. Only root may connect.
pub const SHARED_ENGINE_SOCKET: &str = "/mnt/wsl/contr/docker.sock";

/// Directory on the data guest's storage where the runtime guest publishes what integrated distributions run: the
/// agent and the Docker CLI in `bin/`, Docker CLI plugins in `cli-plugins/`, and the release they came from in
/// `release`.
pub const INTEGRATION_DIR: &str = "/mnt/wsl/contr/data/integration";

/// Directory where each integrated distribution mounts its root file system, at its own name, so the engine can
/// bind paths of that distribution.
pub const DISTROS_DIR: &str = "/mnt/wsl/contr/distros";

/// Line `contr-agent integrate` writes to standard output once the distribution reaches the engine.
pub const INTEGRATION_READY: &str = "contr integration ready";

/// File next to the guest images that names them and their release, as JSON with the keys `release`, `runtime`
/// and `data`.
pub const MANIFEST_FILE: &str = "guest.json";

/// Exit code of `contr-agent configure` when the engine rejects the configuration; standard output then holds a
/// [`ConfigRejection`] as JSON.
pub const CONFIG_REJECTED: u8 = 3;

/// Why the engine rejects a configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigRejection {
    /// The engine's reason.
    pub reason: String,
    /// Top-level `daemon.json` keys the engine does not know, sorted.
    pub unknown_keys: Vec<UnknownKey>,
}

/// A top-level `daemon.json` key the engine does not know.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownKey {
    /// The key as written.
    pub key: String,
    /// A key the engine accepts with the same value.
    pub suggestion: Option<String>,
}

/// What the engine uses from its next start, which `contr-agent configure` reads as JSON on standard input.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineConfig {
    /// The engine's `daemon.json`.
    pub daemon: Map<String, Value>,
    /// PEM certificates the engine trusts besides the guest's own.
    pub certificates: String,
}

/// What `contr-agent integrate` reads as one line of JSON on standard input before it serves the engine.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IntegrationConfig {
    /// Proxy that containers and builds created from the distribution receive, as the guest reaches it.
    pub container_proxy: Option<Proxy>,
}
