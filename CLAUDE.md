# Contr core: working rules

## Clean room (non-negotiable)

- Never inspect, analyze, decompile, attach to, or read files from Docker Desktop: `/opt/docker-desktop`,
  `~/.docker/desktop`, `~/.docker/cli-plugins` binaries it installed, `C:\Program Files\Docker`, `Docker Desktop.app`.
- Never use the rea MCP server, read `~/.rea`, or read the `contr-research` repository from here.
- Behavior comes from `specs/`, Docker's public documentation, and Apache-2.0/MIT projects (Moby, Compose, Rancher
  Desktop, Podman Desktop, Lima, k3s).
- Work against a Docker Engine context (`default` or `contr`), never `desktop-linux`. Run docker-dependent tests with a
  clean `DOCKER_CONFIG`.

## Code

- Comments follow [docs/COMMENTS.md](docs/COMMENTS.md): facts the code cannot show, no narrative, no history. `///` on
  declarations, `//` only inside bodies and at most two lines. Names, types and small functions carry the meaning.
- Every name derived from the product comes from `contr_core::product`.
- `crates/contr-api/src/gen` is generated. Edit `proto/`, bump `proto/API_REVISION`, run `cargo xtask codegen`.
- `thiserror` in libraries, `anyhow` only in binaries. No `unwrap`, `expect`, `panic!` or `todo!` outside tests.
- `unsafe` is denied workspace-wide; only `contr-transport` and `contr-platform` may allow it, each block preceded by a
  `// SAFETY:` fact.
- Contract crates (`contr-core`, `contr-backend`, `contr-transport`, `contr-sdk`) enable `missing_docs`; other crates
  document only what carries an invariant.
- New types instead of primitives for IDs and states; enums make invalid states unrepresentable.

## Proto

- Package `contr.<service>.v1`, file `proto/contr/<service>/v1/<service>.proto`, C# namespace `Contr.Sdk.<Service>.V1`.
- buf STANDARD lint plus a comment on every service and RPC. Field comments only for units, ranges, or values clients
  switch on.
- Never reuse or rename a field number, field, enum value or RPC (see [COMPATIBILITY.md](COMPATIBILITY.md)).

## Checks before every push

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo xtask codegen && git diff --exit-code
buf lint && buf format --diff --exit-code
```

## Git

- Conventional Commits (`feat(daemon): …`, `fix(api): …`). One logical change per commit.
- `main` is protected; changes land through pull requests with green CI.
