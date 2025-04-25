FROM rust:1.70-slim as builder
WORKDIR /app
COPY . .
RUN cargo run --bin mys-faucet -- --host-ip 0.0.0.0 --port 9123 --write-ahead-log ./faucet.wal

EXPOSE 5003
CMD ["./mys-faucet", "--host-ip", "0.0.0.0", "--port", "5003", "--write-ahead-log", "/tmp/faucet-wal"]