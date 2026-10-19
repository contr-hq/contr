# 0004. Kubernetes is k3s running as a container on the active engine

## Context

Kubernetes must work the same on the WSL backend, the Linux host engine and the later VM backend, and must never modify
the user's host beyond its kubeconfig.

## Decision

- Run `rancher/k3s` (pinned tag, mirror overridable) as the privileged container `contr-k3s-server` with its state in
  volume `contr-k3s`, API published on `127.0.0.1:6443`.
- Merge a kubeconfig context named `contr`; switch the current context only when the user asks.
- Disable keeps the volume; reset deletes container and volume.

## Consequences

- One implementation for every backend.
- Locally built images are not visible to the cluster until imported; `ImportImages` and an optional auto-import cover it.
- Podman host mode and rootless Docker do not support the cluster until verified.
