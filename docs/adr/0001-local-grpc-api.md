# 0001. The daemon serves a gRPC API over local IPC only

## Context

Three clients in three languages (C#/WinUI 3, Rust/GTK4, Swift later), the `contr` CLI and the tray all drive the same
engine. The engine must keep running when every window is closed, and the daemon is root-equivalent through the engine
socket, so its API must never be reachable from the network.

## Decision

- `contrd` is a per-user daemon serving gRPC (tonic) on a Unix socket in `$XDG_RUNTIME_DIR/contr/` (mode 0600,
  peer UID checked) and on per-user Windows named pipes (`\\.\pipe\contr-<SID>-api`, DACL limited to the user and
  SYSTEM, remote clients rejected). It never listens on TCP.
- `proto/` is the single contract. `contr-sdkgen` generates the Rust, C# and Swift SDKs from it.
- Packages are split by responsibility: `contr.daemon.v1` (handshake, identity), `contr.engine.v1` (lifecycle), then
  one package per resource. A package named `system` is avoided because its C# namespace would shadow .NET's `System`.

## Consequences

- Clients are thin and identical in behavior; logic lives in the daemon once.
- Every API change goes through buf lint, `buf breaking` and an `API_REVISION` bump.
- .NET connects through `SocketsHttpHandler.ConnectCallback`, so the C# SDK owns reconnection.
