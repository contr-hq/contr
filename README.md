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

- Linux: `contrd` manages a Docker Engine or Podman installed on the host and serves the gRPC API on a per-user
  socket. The `contr` CLI, the Rust SDK and the generated C# SDK talk to it.
- Windows (Phase 2, in progress): `contrd` runs Moby in two Contr-owned WSL 2 distributions and serves the engine on a
  per-user named pipe with the `contr` docker context, and on `\\.\pipe\docker_engine` when no other program
  owns it, so `docker` works without a context. Windows bind paths such as `-v C:\src:/w` work, and
  `host.docker.internal` reaches services on the Windows host, including those bound to `localhost`. CLI
  installation and integration with the user's own distributions are not done yet.

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
| `sdk/csharp/` | Template of the generated C# SDK, its conformance program and samples |
| `guest/` | Root filesystems of the WSL distributions, built by `cargo xtask guest` |
| `packaging/linux/` | systemd user units for socket activation |
| `specs/` | Clean-room specifications |
| `docs/adr/` | Architecture decisions |

## Development

```sh
cargo build --workspace
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo xtask codegen    # after editing proto/
```

Run the daemon and CLI from a build tree; `contr start` starts `contrd` when nothing answers:

```sh
cargo build --package contr-daemon --package contr-cli
target/debug/contr start
target/debug/contr status
docker --context contr ps
```

Build the WSL guest images (needs a Docker Engine with buildx) and try the Windows backend in the
[Windows sandbox](tools/windows-sandbox/README.md):

```sh
cargo xtask guest
tools/windows-sandbox/sandbox put target/guest/* C:/contr-guest
tools/windows-sandbox/sandbox ssh 'cd C:/src/dockercraft; cargo build -p contr-daemon -p contr-cli'
```

Generate the C# SDK and check it against the fake backend (needs the .NET 10 SDK):

```sh
cargo run --package contr-sdkgen -- generate --lang csharp --out target/sdk/csharp
cargo run --package contr-sdkgen -- verify --lang csharp --sdk target/sdk/csharp --daemon target/debug/contrd
```

Contributors follow [CLAUDE.md](CLAUDE.md) and the comment convention in [docs/COMMENTS.md](docs/COMMENTS.md).

## Clean-room policy

Contr is specified from public documentation and open-source projects only. See [specs/README.md](specs/README.md).

## License

Apache-2.0. Docker is a trademark of Docker, Inc. Contr is not affiliated with or endorsed by Docker, Inc.
