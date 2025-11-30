# Build application
FROM rust:1.87-bullseye AS builder
WORKDIR /app

# Install dependencies
RUN apt-get update && apt-get install -y \
    cmake \
    clang \
    libclang-dev \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Copy workspace
COPY . .

# Build the faucet binaries
RUN cargo build --release --bin mys-faucet --bin merge_coins

# Production Image
FROM debian:bullseye-slim AS runtime

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    iputils-ping \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy binaries and setup script
COPY --from=builder /app/target/release/mys-faucet /app/bin/
COPY --from=builder /app/target/release/merge_coins /app/bin/
COPY --from=builder /app/crates/mys-faucet/setup-wallet.sh /app/bin/setup-wallet.sh
RUN chmod +x /app/bin/setup-wallet.sh

# Use the setup script as entrypoint
ENTRYPOINT ["/app/bin/setup-wallet.sh"]

