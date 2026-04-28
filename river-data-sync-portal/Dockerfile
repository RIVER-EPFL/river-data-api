FROM rust:1.93-slim AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

# Copy workspace root files for dependency resolution
COPY Cargo.toml Cargo.lock ./
COPY migration/Cargo.toml migration/Cargo.toml
COPY river-data-sync-common/Cargo.toml river-data-sync-common/Cargo.toml
COPY river-data-sync-vaisala/Cargo.toml river-data-sync-vaisala/Cargo.toml
COPY river-data-toolbox/Cargo.toml river-data-toolbox/Cargo.toml

# Copy the shared crate and portal sync microservice
COPY river-data-sync-common/ river-data-sync-common/
COPY river-data-sync-portal/ river-data-sync-portal/

# Create dummy crates to satisfy workspace
RUN mkdir -p src && echo "fn main() {}" > src/main.rs
RUN mkdir -p migration/src && echo "fn main() {}" > migration/src/main.rs && touch migration/src/lib.rs
RUN mkdir -p river-data-toolbox/src && echo "" > river-data-toolbox/src/lib.rs
RUN mkdir -p river-data-sync-vaisala/src && echo "fn main() {}" > river-data-sync-vaisala/src/main.rs

# Build only the portal sync crate
RUN cargo build --release -p river-data-sync-portal

# Runtime image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/river-data-sync-portal /usr/local/bin/

ENTRYPOINT ["river-data-sync-portal"]
