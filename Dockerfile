
FROM rust:1.88.0 AS builder

WORKDIR /usr/src/valut
COPY . .
RUN cargo install --path .

FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install -y \
    ca-certificates \
    openssl \
    curl && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/cargo/bin/valut /usr/local/bin/valut

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8000/health || exit 1

ENTRYPOINT ["valut"]
