FROM rust:1.93.1 AS chef
RUN cargo install cargo-chef
WORKDIR /usr/src/valut

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /usr/src/valut/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
RUN cargo build --release --bin valut && \
    strip target/release/valut

FROM debian:trixie-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3t64 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/valut/target/release/valut /usr/local/bin/valut

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8000/health || exit 1

ENTRYPOINT ["/usr/local/bin/valut"]
