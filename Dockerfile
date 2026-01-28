# Stage 1: Chef (base with dependencies)
FROM rust:1.93-slim AS chef
RUN cargo install cargo-chef
WORKDIR /app

# Stage 2: Planner (recipe generation)
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Stage 3: Builder (cached build)
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release

# Stage 4: Runtime (minimal image)
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/river-db /usr/local/bin/
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/river-db"]
