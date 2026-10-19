# 0002. Every platform runtime implements one `Backend` contract

## Context

The engine runs on the Linux host, in WSL 2 distributions on Windows, in a QEMU/KVM VM on Linux later, and in memory for
tests. Clients must see one lifecycle regardless of the runtime.

## Decision

- `contr-backend` defines `Backend` (probe, provision, start, stop, reset) and `EngineState`. Each runtime lives in its
  own crate (`contr-backend-host`, `contr-backend-wsl`, `contr-backend-qemu`) plus a fake for tests.
- `start` returns an `EngineConnector`; everything above the backend talks to a plain Docker Engine API.
- Unmet prerequisites are `Blocker`s with `Remediation`s, which the API carries to clients. Onboarding and troubleshooting
  screens render them instead of hard-coding platform checks.

## Consequences

- Clients and SDKs can be built and tested against the fake backend without WSL or a VM.
- A new runtime is a new crate, not a change to the daemon's lifecycle code.
