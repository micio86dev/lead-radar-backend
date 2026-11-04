# Pinned toolchain and base images; no `latest` tags.
FROM rust:1.89.0-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release --locked --bins

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && apt-get clean
WORKDIR /app
COPY --from=builder /app/target/release/api /usr/local/bin/api
COPY --from=builder /app/target/release/worker /usr/local/bin/worker
COPY --from=builder /app/target/release/db /usr/local/bin/db
COPY --from=builder /app/target/release/healthcheck /usr/local/bin/healthcheck
COPY deploy/stack.sh /usr/local/bin/stack
COPY deploy/entrypoint.sh /usr/local/bin/entrypoint
RUN chmod +x /usr/local/bin/stack /usr/local/bin/entrypoint
RUN mkdir -p /data && chown 10001:10001 /data
ENV APP_HOST=0.0.0.0 \
    APP_PORT=8080 \
    RUST_LOG=info
# The container starts as root only to fix the data volume ownership; the entrypoint then drops to
# UID 10001 before running the requested binary. Nothing runs as root after that step.
EXPOSE 8080
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/entrypoint"]
# Compose overrides this with the individual binaries; a single-node host runs the stack entrypoint,
# which applies migrations and seeds before starting the worker and the API.
CMD ["/usr/local/bin/stack"]
