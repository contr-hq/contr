# 0009. Containers and builds receive the engine's proxy from the engine proxy

## Context

The design's "Pass these settings to containers" toggle gives new containers and builds `HTTP_PROXY`, `HTTPS_PROXY`
and `NO_PROXY`, in upper and lower case, from the proxy the engine uses. Programs in a container, and `RUN` steps of a
build, see a proxy only through these variables; `dockerd`'s own `proxies` apply to its pulls and pushes.

- Docker's mechanism is the client's: the Docker CLI reads `proxies` from its `config.json`, keyed by daemon host
  with `default` for any host, and adds the variables to the containers it creates and to the build arguments it sends
  ([Docker docs](https://docs.docker.com/engine/cli/proxy/)). Compose and Buildx read the same key.
- Every client of Contr's engine on Windows already passes through `contr-engine-proxy`: Windows programs through the
  pipes `contrd` serves, and integrated WSL distributions through `contr-agent integrate`, which serves
  `/var/run/docker.sock` in front of the shared engine socket (ADR 0007).
- Containers need the proxy as the guest reaches it: a proxy on Windows' loopback interface is reached at the relay
  address `198.19.255.254`, which exists only in Contr's guest.
- `docker build` runs Buildx, whose `docker` driver does not call `/build`. It upgrades `/grpc` to HTTP/2 and calls
  BuildKit's gRPC API, where build arguments are `build-arg:<NAME>` frontend options of
  `moby.buildkit.v1.frontend.LLBBridge/Solve`; older clients put them in `moby.buildkit.v1.Control/Solve`.

## Decision

- The setting `proxy.pass_to_containers`, off by default, applies at the next engine start like the other proxy
  settings. The engine proxy of every pipe and every integration then receives the proxy the engine uses, in the form
  the guest reaches it. `contrd` hands it to `contr-agent integrate` as JSON on standard input, because a proxy URL
  may carry credentials.
- `contr-engine-proxy` adds the variables to:
  - `Env` of `POST /containers/create`;
  - the `buildargs` of `POST /build`;
  - the frontend options of both Solve calls when they name a frontend. While the setting is on, the proxy relays the
    HTTP/2 connection of `/grpc` frame by frame: it decodes the client's header blocks to find the Solve calls and
    replaces only their request bodies. Every other frame passes unchanged, and a call that declares its length or
    would not fit the engine's flow-control window passes unchanged too.
- A variable the request sets in either case keeps the request's value, and the proxy adds neither case of it.
- `NO_PROXY` holds the user's exclusions, loopback, `host.docker.internal` and the relay address, as the engine's own
  `no-proxy` does.
- Contr does not write the Docker CLI's `config.json`.

Terminating HTTP/2 on both sides was rejected: BuildKit's client on Windows sends an `:authority` that is not a valid
URI authority (its default address, `npipe:////./pipe/buildkitd`, percent-encoded), which `dockerd` accepts and Rust's
`h2` server resets.

Managing `proxies` in `config.json` was rejected:

- It reaches only the Docker CLI, Compose and Buildx, not programs that call the API, such as Testcontainers, IDEs and
  SDKs.
- Contr would edit a file the user owns, in the Windows profile and in the home of every integrated distribution's
  user, merge with what the user set there, race `docker login` writing the same file, and leave entries behind when
  Contr is removed.
- A `default` entry would send other engines to the relay address; an entry per daemon host depends on how each client
  names the endpoint.

## Consequences

- Every client on Windows and in integrated distributions gets the proxy, whether or not it is the Docker CLI.
- A proxy configured in the user's `config.json` still wins: the CLI sets the variables before the request reaches
  the engine proxy.
- Existing containers keep the environment they were created with; turning the setting off removes nothing.
- Builds on a BuildKit outside the engine (Buildx `docker-container`, `kubernetes` and `remote` drivers) get no build
  arguments, because their gRPC travels in streams the proxy does not parse. A `docker-container` builder is created
  through the proxy, so BuildKit itself pulls through the proxy. These builds need `--build-arg` or `config.json`.
- The proxy depends on the field numbers of the two Solve requests, which BuildKit keeps stable for its clients. A
  session whose header blocks it cannot decode keeps working, without build arguments.
- Kubernetes pods are not created through the Docker API and get no variables.
