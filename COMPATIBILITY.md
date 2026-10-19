# Compatibility policy

## Versions

- The core repository releases in lockstep: one tag `vX.Y.Z` versions the daemon, CLI, guest agent, guest image and
  every SDK (`Contr.Sdk` on NuGet, `contr-api` and `contr-sdk` crates, `ContrSDK` for Swift).
- `proto/API_REVISION` is a single integer that increases with every change to a public `.proto` file. The daemon
  reports it in `DaemonService.Handshake`, and every SDK embeds the value it was generated from.

## Rules

1. A client generated at revision R works with every daemon at revision R or later.
2. Against an older daemon, an RPC the daemon lacks returns `UNIMPLEMENTED`; SDKs surface it as `FeatureUnavailable`.
   Clients decide what to show from `HandshakeResponse.capabilities`, never from version numbers.
3. A breaking change creates a new package version (`contr.<service>.v2`) served beside `v1` for at least two minor
   releases. `buf breaking` rejects any other breaking change.
4. Field numbers, field names, enum values and RPC names are never reused or renamed.
5. `proto-internal/` (daemon ↔ guest agent) is outside this policy. The daemon redeploys the agent when the versions
   differ.

## Clients

- `contr-windows` bundles a pinned core release (`core.lock.json`: version and SHA-256).
- `contr-linux` packages depend on `contr-daemon >= X.Y`.
- Both client repositories build nightly against core `main` to catch contract drift early.
