# Contr

Contr is a desktop container platform for Windows and Linux. It runs the open-source Moby engine and works with the
standard Docker CLI, Docker Compose and the Docker Engine API, so existing workflows keep working. It is built for
organizations that need a self-hosted, Apache-2.0 container desktop.

This repository is the core engine: the `contrd` daemon, the `contr` CLI, the guest agent, the gRPC contract and the SDK
generator. The desktop clients live in their own repositories and talk to the daemon only through the generated SDKs.

| Repository | Contents |
|---|---|
| [contr](https://github.com/contr-hq/contr) | Core engine (this repository) |
| [contr-windows](https://github.com/contr-hq/contr-windows) | WinUI 3 client |
| [contr-linux](https://github.com/contr-hq/contr-linux) | GTK4 and libadwaita client for Fedora and Ubuntu |
| [contr-macos](https://github.com/contr-hq/contr-macos) | macOS client (planned) |

## Status

Phase 0: workspace, API contract, project rules. Nothing is usable yet.

## Layout

| Path | Contents |
|---|---|
| `proto/` | gRPC contract; `proto/API_REVISION` increases with every change |
| `crates/contr-core` | Product identity, error reasons, remediations |
| `crates/contr-api` | Rust code generated from `proto/` |
| `crates/contr-backend` | Backend contract every platform runtime implements |
| `crates/contr-daemon` | `contrd` |
| `crates/contr-cli` | `contr` |
| `crates/contr-sdkgen` | `contr-sdkgen`, the SDK generator |
| `crates/*` | Remaining crates, one per responsibility; see each crate's description |
| `specs/` | Clean-room specifications |
| `docs/adr/` | Architecture decisions |

## Development

```sh
cargo build --workspace
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo xtask codegen    # after editing proto/
```

Contributors follow [CLAUDE.md](CLAUDE.md) and the comment convention in [docs/COMMENTS.md](docs/COMMENTS.md).

## Clean-room policy

Contr is specified from public documentation and open-source projects only. See [specs/README.md](specs/README.md).

## License

Apache-2.0. Docker is a trademark of Docker, Inc. Contr is not affiliated with or endorsed by Docker, Inc.
