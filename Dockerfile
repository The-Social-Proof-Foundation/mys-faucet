# Build application
FROM rust:1.87-bullseye AS builder

# Install dependencies
RUN apt-get update && apt-get install -y \
    cmake \
    clang \
    libclang-dev \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Copy build context - Railway may send repo root or subdirectory
COPY . /build-context/

# Find workspace root and set up build directory
WORKDIR /app
RUN set -e; \
    if [ -f /build-context/Cargo.toml ] && [ -d /build-context/crates ]; then \
        echo "Build context is workspace root"; \
        cp -r /build-context/* /app/ 2>/dev/null || true; \
        cp -r /build-context/.[!.]* /app/ 2>/dev/null || true; \
    elif [ -d /build-context/crates ]; then \
        echo "Build context has crates directory"; \
        cp -r /build-context/* /app/ 2>/dev/null || true; \
        cp -r /build-context/.[!.]* /app/ 2>/dev/null || true; \
    else \
        echo "Build context structure:"; \
        ls -la /build-context/ | head -20; \
        echo "Searching for workspace root..."; \
        WORKSPACE_ROOT=$(find /build-context -maxdepth 5 -name "Cargo.toml" -type f ! -path "*/target/*" | head -1 | xargs dirname 2>/dev/null || echo ""); \
        if [ -n "$WORKSPACE_ROOT" ] && [ "$WORKSPACE_ROOT" != "/build-context" ] && [ -d "$WORKSPACE_ROOT/crates" ]; then \
            echo "Found workspace root at $WORKSPACE_ROOT"; \
            cp -r "$WORKSPACE_ROOT"/* /app/ 2>/dev/null || true; \
            cp -r "$WORKSPACE_ROOT"/.[!.]* /app/ 2>/dev/null || true; \
        elif [ -f /build-context/Cargo.toml ] && [ ! -d /build-context/crates ]; then \
            echo "ERROR: Build context is missing workspace root (crates/ and external-crates/ directories)."; \
            echo "Build context only contains:"; \
            ls -la /build-context/; \
            echo ""; \
            echo "The workspace root Cargo.toml (with workspace.dependencies) is required to build."; \
            echo "This crate depends on workspace dependencies that are defined in the root Cargo.toml."; \
            exit 1; \
        else \
            echo "ERROR: Cannot locate workspace root"; \
            echo "Build context contents:"; \
            ls -la /build-context/; \
            echo "Cargo.toml files found:"; \
            find /build-context -name "Cargo.toml" -type f | head -10; \
            exit 1; \
        fi; \
    fi

# Verify workspace root
RUN test -f Cargo.toml && test -d crates && test -d external-crates || \
    (echo "ERROR: Workspace root verification failed - missing crates/ or external-crates/ directories" && \
     echo "Current directory contents:" && ls -la && \
     echo "The workspace root structure is required to resolve workspace dependencies." && exit 1)

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

