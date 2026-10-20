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
- Named pipes cannot half-close, so the engine pipe runs in message mode and an empty message ends a direction, the
  convention Docker clients on Windows follow.
- Measured in the nested Windows sandbox: `wsl.exe` standard streams carry about 49 MiB/s, and the relay delivers
  11–17 MiB/s of container output. Large transfers (`docker load`, big build contexts) are the case for Hyper-V
  sockets.
- `host.docker.internal` resolves to a reserved guest address; the agent redirects that traffic and `contrd` opens
  the connection on the Windows loopback interface. Containers therefore reach the user's local services without a
  firewall rule, with the user's network identity, as Docker clients expect.
