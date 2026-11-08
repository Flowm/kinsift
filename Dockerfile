# syntax=docker/dockerfile:1
#
# Targets:
#   binary   static Linux binary only, for `--output type=local,dest=dist`
#   runtime  (default) small Alpine image with the binary as entrypoint
#
# The build stage always runs on the build machine's platform and
# cross-compiles to TARGETPLATFORM with tonistiigi/xx, so arm64 images
# build without QEMU emulation.

ARG RUST_VERSION=1.99.1
ARG ALPINE_VERSION=3.22

FROM --platform=$BUILDPLATFORM tonistiigi/xx:1.6.1 AS xx

FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS build
COPY --from=xx / /
RUN apk add --no-cache clang lld
ARG TARGETPLATFORM
# C toolchain pieces for the target (bundled SQLite is compiled from C).
RUN xx-apk add --no-cache musl-dev gcc
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=kinsift-target-$TARGETPLATFORM \
    xx-cargo build --release --locked -p kinsift \
    && cp "target/$(xx-cargo --print-target-triple)/release/kinsift" /kinsift \
    && xx-verify --static /kinsift

FROM scratch AS binary
COPY --from=build /kinsift /kinsift

FROM alpine:${ALPINE_VERSION} AS runtime
COPY --from=build /kinsift /usr/local/bin/kinsift
# Index and config live in mounted volumes. Run with `--user "$(id -u):$(id -g)"`
# so kinsift acts on your files with your permissions.
ENV HOME=/data \
    KINSIFT_INDEX=/data/index.db \
    KINSIFT_CONFIG=/config/config.toml
RUN mkdir -p /data /config && chmod 1777 /data
VOLUME ["/data"]
WORKDIR /data
ENTRYPOINT ["kinsift"]
CMD ["--help"]
