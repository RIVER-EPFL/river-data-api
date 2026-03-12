FROM rust:1.93-slim AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

# Copy workspace root files for dependency resolution
COPY Cargo.toml Cargo.lock ./
COPY migration/Cargo.toml migration/Cargo.toml

# Copy the sync microservice
COPY river-data-sync-vaisala/ river-data-sync-vaisala/

# Create dummy main crate src to satisfy workspace
RUN mkdir -p src && echo "fn main() {}" > src/main.rs
RUN mkdir -p migration/src && echo "fn main() {}" > migration/src/main.rs && touch migration/src/lib.rs

# Build only the sync crate (and cache dependencies)
RUN cargo build --release -p river-data-sync-vaisala

# Runtime image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/river-data-sync-vaisala /usr/local/bin/

ENTRYPOINT ["river-data-sync-vaisala"]
