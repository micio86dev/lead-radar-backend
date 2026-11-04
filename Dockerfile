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
RUN chmod +x /usr/local/bin/stack
RUN mkdir -p /data && chown 10001:10001 /data
ENV APP_HOST=0.0.0.0 \
    APP_PORT=8080 \
    RUST_LOG=info
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/bin/tini", "--"]
# Compose overrides this with the individual binaries; a single-node host runs the stack entrypoint,
# which applies migrations and seeds before starting the worker and the API.
CMD ["/usr/local/bin/stack"]
