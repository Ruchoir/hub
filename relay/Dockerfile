# The push relay (docs/push-relay.md), run by the publisher, apart from every instance's hub. Built
# from the repository root:
#   docker build -f relay/Dockerfile -t ruchoir-push-relay:<version> .
# Cargo loads every workspace member's manifest, so the hub's are copied too; only the relay is
# compiled.

FROM rust:1.96-slim-bookworm AS build
WORKDIR /build
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY src ./src
COPY migration ./migration
COPY relay ./relay
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p ruchoir-push-relay \
    && cp target/release/ruchoir-push-relay /usr/local/bin/ruchoir-push-relay

# User and group 10002, fixed: the keys' directory on the host is given to that group.
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10002 relay \
    && useradd --system --uid 10002 --gid 10002 relay
COPY --from=build /usr/local/bin/ruchoir-push-relay /usr/local/bin/ruchoir-push-relay
ENV RELAY_BIND=0.0.0.0:8080 \
    RUST_LOG=info
USER relay
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD curl -fsS http://127.0.0.1:8080/health || exit 1
CMD ["ruchoir-push-relay"]
