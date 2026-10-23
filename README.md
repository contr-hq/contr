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
  socket. The `contr` CLI, the Rust SDK and the generated C# SDK talk to it. Without an engine, `contr fix
  install-engine` installs the distribution's own engine behind one password prompt. Releases carry the
  `contr-daemon` package as `.deb` and `.rpm`.
- Windows: `contrd` runs Moby in two Contr-owned WSL 2 distributions and serves the engine on a
  per-user named pipe with the `contr` docker context, and on `\\.\pipe\docker_engine` when no other program
  owns it, so `docker` works without a context. Windows bind paths such as `-v C:\src:/w` work, and
  `host.docker.internal` reaches services on the Windows host, including those bound to `localhost`. A port a
  container publishes listens on Windows at the address Docker reports: `-p 8080:80` on every interface, and
  `-p 127.0.0.1:8080:80` on loopback only. Other computers reach a published port once Windows Firewall allows it;
  see [Published ports and Windows Firewall](#published-ports-and-windows-firewall).
  `contr cli install` installs the bundled Docker CLI, Compose and Buildx for the user. `contr wsl enable <name>`
  gives one of the user's own WSL 2 distributions `docker`, Compose and Buildx on the same engine, with binds of
  the distribution's paths.
- Both: Compose projects are grouped from the engine's containers, and `contr compose` runs their actions; a
  single-node k3s cluster runs on the engine through `contr kube`. Pulls, pushes, Compose actions, image imports,
  diagnostics bundles, fixes and engine operations run as operations the daemon owns, so they go on after the app or
  command that started them goes away, and every client can follow and cancel them
  ([ADR 0010](docs/adr/0010-daemon-owned-operations.md)).
  `contr settings` shows and changes the daemon's settings, and `contr doctor` checks what keeps the engine
  from working and offers fixes that `contr fix` runs. `contr bundle` saves a diagnostics zip for support, with
  credentials and the user's name, home directory and computer name replaced. The daemon checks the published releases
  once a day through the configured proxy, and `contr update` checks now.

## Settings managed by an administrator

An administrator fixes settings for every user of a computer with a JSON object shaped like the user's settings
file, for example `{"engine": {"serveDefaultPipe": false}}`, or `{"updates": {"check": false}}` to stop update
checks. On Windows it is the `Settings` string value of `HKEY_LOCAL_MACHINE\SOFTWARE\Policies\Contr`; on Linux it is
`/etc/contr/admin-settings.json`. Clients show those settings as managed by the organization, and the daemon refuses
changes to them.

## Published ports and Windows Firewall

Contr installs for one user without administrator rights and adds no firewall rule. The first time a container
publishes a port on every interface, Windows asks whether Contr daemon may accept connections from networks. A
standard user cannot allow it, and Windows then blocks inbound connections to `contrd.exe`. The port still answers on
the computer itself; other computers reach it once an administrator allows it:

1. Open Windows Defender Firewall with Advanced Security (`wf.msc`) as an administrator.
2. Under Inbound Rules, open each Contr daemon rule whose action is Block and whose profile covers the networks that
   should reach published ports, usually Private, and choose Allow the connection.

A Block rule wins over every Allow rule for the same program, so a separate Allow rule is not enough. An administrator
who answers the first prompt allows it there.

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
| `packaging/windows/` | Docker CLI tools bundled for Windows, fetched by `cargo xtask windows-cli` |
| `packaging/linux/` | systemd user units, the polkit policy, and the Debian and RPM packages built by `cargo xtask package-linux` |
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
cargo xtask windows-cli
tools/windows-sandbox/sandbox put target/guest/* C:/contr-guest
tools/windows-sandbox/sandbox ssh 'cd C:/src/dockercraft; cargo build -p contr-daemon -p contr-cli'
```

Generate the C# SDK and check it against the fake backend (needs the .NET 10 SDK):

```sh
cargo run --package contr-sdkgen -- generate --lang csharp --out target/sdk/csharp
cargo run --package contr-sdkgen -- verify --lang csharp --sdk target/sdk/csharp --daemon target/debug/contrd
```

Releases and their signing are described in [docs/releasing.md](docs/releasing.md); [Verifying downloads](docs/verifying.md)
shows how to check a release file, and the [Code signing policy](docs/code-signing-policy.md) says what is signed, by
whom, and what Contr sends over the network.

Contributors follow [CLAUDE.md](CLAUDE.md) and the comment convention in [docs/COMMENTS.md](docs/COMMENTS.md).

## Clean-room policy

Contr is specified from public documentation and open-source projects only. See [specs/README.md](specs/README.md).

## License

Apache-2.0. Docker is a trademark of Docker, Inc. Contr is not affiliated with or endorsed by Docker, Inc.
