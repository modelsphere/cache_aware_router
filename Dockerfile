# Both base images are build args, so a build behind a registry mirror (or on a
# runner that cannot reach Docker Hub) can point them at its own registry.
#
# Public build — everything from Docker Hub, nothing else needed:
#   docker build -t cache-aware-router:dev .
#
# Behind a slow or restricted network, also pass a crates.io mirror:
#   docker build --build-arg CARGO_REGISTRY="sparse+https://rsproxy.cn/index/" .
#
# See .gitlab-ci.yml for how the internal CI overrides these.

ARG BUILDER_IMAGE=rust:1.88-bookworm
ARG RUNTIME_IMAGE=debian:12-slim

# ---------- builder ----------
FROM ${BUILDER_IMAGE} AS builder

# Optional crates.io mirror. Empty (the default) means crates.io directly, and
# also leaves any cargo config already baked into a custom builder image alone.
ARG CARGO_REGISTRY=""
RUN set -eux; \
    if [ -n "$CARGO_REGISTRY" ]; then \
        CH="${CARGO_HOME:-$HOME/.cargo}"; \
        mkdir -p "$CH"; \
        printf '[source.crates-io]\nreplace-with = "mirror"\n\n[source.mirror]\nregistry = "%s"\n' \
            "$CARGO_REGISTRY" > "$CH/config.toml"; \
    fi

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# ---------- runtime ----------
# No apt here, so the runtime image can come from a registry mirror with no
# package feed reachable. TLS is rustls (reqwest 0.13), so nothing links OpenSSL
# — verified with ldd: the binary needs only libgcc/libpthread/libm/libdl/libc.
# What it does need is the system trust store, because rustls-native-certs reads
# it at startup; that is copied from the builder.
FROM ${RUNTIME_IMAGE}

COPY --from=builder /etc/ssl/certs /etc/ssl/certs

WORKDIR /workspace
COPY --from=builder /src/target/release/cache-aware-router ./cache-aware-router
COPY config.example.yaml ./config.example.yaml
COPY launch_service ./launch_service
RUN chmod +x ./launch_service ./cache-aware-router

EXPOSE 6700
ENTRYPOINT ["./launch_service"]
