# Feature inventory

Release: **MVP** ships in 1.0; **Later** is planned after 1.0. Spec links are added as each area is specified.

## Engine

| ID | Feature | Release | Spec |
|---|---|---|---|
| ENG-01 | Provision, start, stop, restart the engine | MVP | — |
| ENG-02 | Linux: manage the host Docker Engine (rootful, rootless, Podman compatibility) | MVP | — |
| ENG-03 | Windows: engine in Contr-owned WSL 2 distributions | MVP | — |
| ENG-04 | Docker CLI context `contr` and optional default pipe/socket | MVP | — |
| ENG-05 | Windows bind-path translation and `host.docker.internal` | MVP | — |
| ENG-06 | Engine access from the user's WSL distributions | MVP | — |
| ENG-07 | Proxy and corporate CA configuration | MVP | — |
| ENG-08 | Engine `daemon.json` editor with validation | MVP | — |
| ENG-09 | Resource limits (CPU, memory, disk) | Later | — |
| ENG-10 | Linux VM backend (QEMU/KVM) | Later | — |
| ENG-11 | Own port forwarder and VPN-safe networking | Later | — |

## Resources

| ID | Feature | Release | Spec |
|---|---|---|---|
| RES-01 | Containers: list, inspect, run, start, stop, restart, pause, kill, remove, prune | MVP | — |
| RES-02 | Container logs: follow, search, timestamps, save | MVP | — |
| RES-03 | Container exec terminal | MVP | — |
| RES-04 | Container stats | MVP | — |
| RES-05 | Images: list, inspect, pull, push, tag, remove, prune | MVP | — |
| RES-06 | Volumes: list, inspect, create, remove, prune | MVP | — |
| RES-07 | Networks: list, inspect, create, remove, connect | MVP | — |
| RES-08 | Compose projects: grouping, up, down, restart, logs | MVP | — |
| RES-09 | Container file browser and copy | Later | — |
| RES-10 | Build history | Later | — |

## Kubernetes

| ID | Feature | Release | Spec |
|---|---|---|---|
| K8S-01 | Enable, disable, reset a single-node cluster | MVP | — |
| K8S-02 | kubeconfig context `contr` | MVP | — |
| K8S-03 | Import local images into the cluster | MVP | — |

## Application

| ID | Feature | Release | Spec |
|---|---|---|---|
| APP-01 | First-run onboarding with prerequisite remediation | MVP | — |
| APP-02 | Tray icon with engine status and actions | MVP | — |
| APP-03 | Settings with administrator-locked keys | MVP | — |
| APP-04 | Troubleshoot: checks, diagnostics bundle, restart, factory reset | MVP | — |
| APP-05 | Notifications | MVP | — |
| APP-06 | Update check (Windows: apply) | MVP | — |
| APP-07 | Enterprise policy (registry/HKLM, `/etc/contr`), registry mirror and allow-list | Later | — |
| APP-08 | Extensions | Later | — |
