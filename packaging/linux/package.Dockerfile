# syntax=docker/dockerfile:1.10
# Debian and RPM packages built by nfpm from nfpm.yaml; `cargo xtask package-linux` passes the archive as the
# `archive` build context and exports the packages.
ARG NFPM=goreleaser/nfpm:v2.47.0@sha256:a662cb167d7b6d3a83920c83d76b12d02b8ac5dd2c13e5c62c15270b23f6df0c

FROM ${NFPM} AS package
ARG VERSION
WORKDIR /package
COPY --from=archive . archive/
COPY . linux/
RUN set -eu; \
    mkdir /out; \
    for packager in deb rpm; do \
      nfpm package --config linux/nfpm.yaml --packager "$packager" --target /out/; \
    done

FROM scratch
COPY --from=package /out/ /
