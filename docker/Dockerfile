# SPDX-License-Identifier: AGPL-3.0-only
# syntax=docker/dockerfile:1.6

# ---- builder ----
FROM rust:1.88-bookworm AS builder
WORKDIR /build

# Pre-fetch crates so source edits don't bust this layer.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/eros-engine-core/Cargo.toml      crates/eros-engine-core/
COPY crates/eros-engine-llm/Cargo.toml       crates/eros-engine-llm/
COPY crates/eros-engine-store/Cargo.toml     crates/eros-engine-store/
COPY crates/eros-engine-server/Cargo.toml    crates/eros-engine-server/
RUN mkdir -p \
        crates/eros-engine-core/src \
        crates/eros-engine-llm/src \
        crates/eros-engine-store/src \
        crates/eros-engine-server/src && \
    echo 'pub fn _stub() {}'           >  crates/eros-engine-core/src/lib.rs && \
    echo 'pub fn _stub() {}'           >  crates/eros-engine-llm/src/lib.rs  && \
    echo 'pub fn _stub() {}'           >  crates/eros-engine-store/src/lib.rs && \
    echo 'fn main() {}'                >  crates/eros-engine-server/src/main.rs && \
    cargo fetch --locked && \
    rm -rf crates/*/src

# Now the real source.
COPY crates ./crates
COPY examples ./examples

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p eros-engine-server && \
    cp target/release/eros-engine /eros-engine

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates libssl3 curl && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /eros-engine /usr/local/bin/eros-engine
COPY examples/model_config.toml /etc/eros-engine/model_config.toml

# sqlx::migrate!() embeds migration files at compile time, but the
# release_command will also need them resolvable from CWD just in case.
COPY crates/eros-engine-store/migrations /etc/eros-engine/migrations

ENV BIND_ADDR=0.0.0.0:8080 \
    MODEL_CONFIG_PATH=/etc/eros-engine/model_config.toml \
    RUST_LOG=info

EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/eros-engine"]
CMD ["serve"]
