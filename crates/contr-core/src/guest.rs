//! Layout of the Contr guest (WSL distributions and virtual machines) that `contrd` and `contr-agent` both rely on.
//! Both binaries ship in one release, so these values change only together with the guest image.

/// Absolute path of the agent inside every guest image.
pub const AGENT_PATH: &str = "/usr/local/bin/contr-agent";

/// File holding the release the guest image was built from; `contrd` replaces a runtime whose release differs.
pub const RELEASE_FILE: &str = "/etc/contr-release";

/// Docker Engine API socket inside the runtime guest.
pub const ENGINE_SOCKET: &str = "/var/run/docker.sock";

/// Directory inside the data guest that holds engine data.
pub const DATA_SOURCE: &str = "/var/lib/contr-data";

/// Mount point of the data guest's storage, shared by every distribution of the WSL 2 virtual machine.
pub const DATA_MOUNT: &str = "/mnt/wsl/contr/data";

/// Line the data anchor writes to standard output once [`DATA_MOUNT`] is in place.
pub const DATA_READY: &str = "contr-data ready";

/// File next to the guest images that names them and their release, as JSON with the keys `release`, `runtime`
/// and `data`.
pub const MANIFEST_FILE: &str = "guest.json";
