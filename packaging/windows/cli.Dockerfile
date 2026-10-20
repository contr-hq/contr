# syntax=docker/dockerfile:1.10
# Docker CLI tools for Windows that contrd installs per user; `cargo xtask windows-cli` exports them. Keep the
# versions in step with guest/Dockerfile.
ARG ALPINE=alpine:3.24@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6

FROM ${ALPINE} AS fetch
ARG DOCKER_VERSION=29.9.1
ARG COMPOSE_VERSION=5.6.0
ARG BUILDX_VERSION=0.38.0
ARG CREDENTIAL_HELPERS_VERSION=0.9.10
RUN set -eu; \
    wget -q -O /tmp/docker.zip "https://download.docker.com/win/static/stable/x86_64/docker-$DOCKER_VERSION.zip"; \
    wget -q -O /tmp/docker-compose.exe \
      "https://github.com/docker/compose/releases/download/v$COMPOSE_VERSION/docker-compose-windows-x86_64.exe"; \
    wget -q -O /tmp/docker-buildx.exe \
      "https://github.com/docker/buildx/releases/download/v$BUILDX_VERSION/buildx-v$BUILDX_VERSION.windows-amd64.exe"; \
    wget -q -O /tmp/docker-credential-wincred.exe \
      "https://github.com/docker/docker-credential-helpers/releases/download/v$CREDENTIAL_HELPERS_VERSION/docker-credential-wincred-v$CREDENTIAL_HELPERS_VERSION.windows-amd64.exe"; \
    printf '%s  %s\n' \
      5d9b66509390553fd97d590ec2ff0ce10b04c2ac653fa52f2abf2f05c4c197ca /tmp/docker.zip \
      8aeaa0055e660e12cf07cd70680dc8333957fe6718528b91ec3da084eab62483 /tmp/docker-compose.exe \
      b39784aed5b3f9b24d1971bbcbad8c8206033fa63ed4fea82c9141728fe0ea26 /tmp/docker-buildx.exe \
      04833862d29a06a6f9353ef0619b7385f37ee3ce1650de5ad3ebe9c3f2ebe233 /tmp/docker-credential-wincred.exe \
      | sha256sum -c -; \
    mkdir -p /tmp/zip /out/bin /out/cli-plugins; \
    unzip -q /tmp/docker.zip docker/docker.exe -d /tmp/zip; \
    install -m 0755 /tmp/zip/docker/docker.exe /tmp/docker-credential-wincred.exe /out/bin/; \
    install -m 0755 /tmp/docker-compose.exe /tmp/docker-buildx.exe /out/cli-plugins/; \
    printf '{\n  "docker": "%s",\n  "compose": "%s",\n  "buildx": "%s",\n  "credential-wincred": "%s"\n}\n' \
      "$DOCKER_VERSION" "$COMPOSE_VERSION" "$BUILDX_VERSION" "$CREDENTIAL_HELPERS_VERSION" > /out/cli.json

FROM scratch
COPY --from=fetch /out/ /
