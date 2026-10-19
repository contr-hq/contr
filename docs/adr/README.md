# Architecture decisions

Each record states Context → Decision → Consequences. A superseded record stays and links to its replacement.

| ADR | Decision |
|---|---|
| [0001](0001-local-grpc-api.md) | The daemon serves a gRPC API over local IPC only |
| [0002](0002-backend-contract.md) | Every platform runtime implements one `Backend` contract |
| [0003](0003-wsl-two-distributions.md) | Windows runs the engine in two Contr-owned WSL 2 distributions |
| [0004](0004-kubernetes-k3s-container.md) | Kubernetes is k3s running as a container on the active engine |
| [0005](0005-windows-installer-velopack.md) | Windows ships through Velopack |
| [0006](0006-product-identity.md) | Product-derived names come from one module |
