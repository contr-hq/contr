# 0003. Windows runs the engine in two Contr-owned WSL 2 distributions

## Context

Windows needs a Linux kernel for the engine. WSL 2 is present on supported Windows versions, needs no administrator
rights to import a distribution, and shares one VM across distributions. Engine data must survive runtime upgrades and
resets.

## Decision

- `contr` holds the runtime (Alpine, Moby static binaries, Compose and Buildx plugins, `contr-agent`) and is replaced on
  every upgrade. `contr-data` holds `/var/lib/docker` and k3s data and is never replaced by upgrades. Rancher Desktop
  uses the same split.
- Distributions are installed with `wsl --import … --version 2`, which works on every WSL 2 build without an OOBE step.
- The daemon talks to `contr-agent` through a yamux session over `wsl.exe` stdio. Hyper-V sockets are adopted only if a
  measured throughput benchmark requires them, because registering their service GUID needs administrator rights once.
- Windows clients reach the engine through `\\.\pipe\contr-<SID>-engine`, served by `contr-engine-proxy`, which rewrites
  Windows bind paths and adds `host.docker.internal`.

## Consequences

- Only installing WSL itself needs elevation.
- Users see two extra entries in `wsl -l`; `wsl --shutdown` stops the engine and the daemon must recover.
