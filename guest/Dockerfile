# syntax=docker/dockerfile:1.10
# Root filesystems of the two Contr WSL distributions; `cargo xtask guest` builds and exports them.
ARG ALPINE=alpine:3.24@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6
ARG RUST=rust:1.98.1-alpine3.24@sha256:7cc1c22d77d9432f7fe012a70e6d3e555af54c2a6832700ed7d553f1769ae89f

FROM ${RUST} AS agent
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --package contr-agent \
    && install -D -m 0755 target/release/contr-agent /out/contr-agent

FROM ${ALPINE} AS engine
ARG TARGETARCH
ARG MOBY_VERSION=29.9.1
ARG COMPOSE_VERSION=5.6.0
ARG BUILDX_VERSION=0.38.0
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) moby_arch=x86_64; plugin_arch=x86_64; buildx_arch=amd64; \
             moby_sha=c158ecc55adb3afb01bbd3539ed3b997d20c26dbdc418080506cc1bf5243ff57; \
             compose_sha=40343e21ca777173e69cff5dbafeb37c6f81f3b0d57d9e597f036e95eb63e76a; \
             buildx_sha=4fe4cc38adf48169132749b6ca22a990928db0118e3407584ee553723115d287 ;; \
      arm64) moby_arch=aarch64; plugin_arch=aarch64; buildx_arch=arm64; \
             moby_sha=3e93cc6d1a686456a3b18eefdde26467a837a7a2b257af22e27cba1dbc1b2018; \
             compose_sha=733ec76717ceb59052a9609b9dadfb523b2df8eab57a54212872d10a58078ea2; \
             buildx_sha=38e890e1a162bfdbf32fbe91404991dc4fc28e55b0de901b34374a061bab4d2c ;; \
      *) echo "unsupported architecture $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    wget -q -O /tmp/moby.tgz "https://download.docker.com/linux/static/stable/$moby_arch/docker-$MOBY_VERSION.tgz"; \
    wget -q -O /tmp/compose "https://github.com/docker/compose/releases/download/v$COMPOSE_VERSION/docker-compose-linux-$plugin_arch"; \
    wget -q -O /tmp/buildx "https://github.com/docker/buildx/releases/download/v$BUILDX_VERSION/buildx-v$BUILDX_VERSION.linux-$buildx_arch"; \
    printf '%s  %s\n' "$moby_sha" /tmp/moby.tgz "$compose_sha" /tmp/compose "$buildx_sha" /tmp/buildx | sha256sum -c -; \
    mkdir -p /out/bin /out/libexec/docker/cli-plugins; \
    tar -xzf /tmp/moby.tgz -C /out/bin --strip-components=1; \
    install -m 0755 /tmp/compose /out/libexec/docker/cli-plugins/docker-compose; \
    install -m 0755 /tmp/buildx /out/libexec/docker/cli-plugins/docker-buildx

FROM ${ALPINE} AS runtime
ARG CONTR_RELEASE
RUN apk add --no-cache ca-certificates iptables ip6tables nftables pigz \
    && printf '%s\n' "$CONTR_RELEASE" > /etc/contr-release
COPY --from=engine /out/ /usr/local/
COPY --from=agent /out/contr-agent /usr/local/bin/contr-agent
COPY guest/runtime/ /

FROM ${ALPINE} AS data
ARG CONTR_RELEASE
RUN mkdir -p /var/lib/contr-data \
    && printf '%s\n' "$CONTR_RELEASE" > /etc/contr-release
COPY --from=agent /out/contr-agent /usr/local/bin/contr-agent
COPY guest/data/ /
