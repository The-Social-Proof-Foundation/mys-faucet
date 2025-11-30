# Build application
FROM rust:1.87-bullseye AS builder
ARG PROFILE=release
ARG GIT_REVISION
ENV GIT_REVISION=$GIT_REVISION
WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    cmake \
    clang \
    libclang-dev \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Copy source code
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

# Copy binaries
COPY --from=builder /app/target/release/mys-faucet /app/bin/
COPY --from=builder /app/target/release/merge_coins /app/bin/

# Copy setup script
COPY crates/mys-faucet/setup-wallet.sh /app/bin/setup-wallet.sh
RUN chmod +x /app/bin/setup-wallet.sh

ARG BUILD_DATE
ARG GIT_REVISION
LABEL build-date=$BUILD_DATE
LABEL git-revision=$GIT_REVISION

# Use the setup script as entrypoint
ENTRYPOINT ["/app/bin/setup-wallet.sh"]

