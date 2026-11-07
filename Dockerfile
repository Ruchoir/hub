# syntax=docker/dockerfile:1.7
# The hub image: the release binary on a slim Debian, run by an unprivileged user.
#
# BuildKit cache mounts keep the cargo registry and the target directory between builds, so a rebuild
# only compiles what changed. The build context is the repository root.

FROM rust:1.96-slim-bookworm AS build
WORKDIR /build
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY src ./src
COPY migration ./migration
# Cargo loads every workspace member's manifest: the relay's too, though it is not compiled here.
COPY relay ./relay
# The binary is copied out of the cached target in the same layer: cache mounts are not part of the
# image.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked --bin hub \
    && cp target/release/hub /usr/local/bin/hub

FROM debian:bookworm-slim AS runtime
# `curl` answers the health check below; `ca-certificates` lets the service reach other services
# over TLS.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group hub
COPY --from=build /usr/local/bin/hub /usr/local/bin/hub
ENV HUB_LISTEN=0.0.0.0:8080 \
    RUST_LOG=info
USER hub
EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s \
    CMD curl -fsS http://127.0.0.1:8080/healthz > /dev/null || exit 1
CMD ["hub"]
