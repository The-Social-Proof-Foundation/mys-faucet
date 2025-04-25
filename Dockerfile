FROM rust:1.70-slim as builder
WORKDIR /app
COPY . .

# Create the deps directories
RUN mkdir -p deps/mys-json-rpc-types deps/mys-types deps/mys-config deps/mys-keys deps/mys-sdk \
    deps/mysten-metrics deps/telemetry-subscribers deps/typed-store deps/shared-crypto \
    deps/mysten-network deps/test-cluster

# Create minimal placeholder Cargo.toml files for each dependency
RUN for dir in deps/*; do \
    echo "[package]" > $dir/Cargo.toml; \
    echo "name = \"$(basename $dir)\"" >> $dir/Cargo.toml; \
    echo "version = \"0.1.0\"" >> $dir/Cargo.toml; \
    echo "edition = \"2021\"" >> $dir/Cargo.toml; \
    mkdir -p $dir/src; \
    echo "// Placeholder lib.rs" > $dir/src/lib.rs; \
    done

# Create a minimal implementation for each dependency
RUN for dir in deps/*; do \
    echo "pub fn init() {}" >> $dir/src/lib.rs; \
    done

# Build using our simplified main.rs
RUN cargo build --release

FROM debian:bullseye-slim
WORKDIR /app
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/mys-faucet .
EXPOSE 5003

# Use a shell script to properly expand environment variables
RUN echo '#!/bin/sh\n\
./mys-faucet --host-ip 0.0.0.0 --port ${PORT:-5003} --write-ahead-log /tmp/faucet-wal\n\
' > /app/start.sh && chmod +x /app/start.sh

ENV PORT=5003
CMD ["/app/start.sh"]