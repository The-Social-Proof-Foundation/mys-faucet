FROM rust:1.70-slim as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bullseye-slim
WORKDIR /app
COPY --from=builder /app/target/release/mys-faucet .
EXPOSE 5003
CMD ["./mys-faucet", "--host-ip", "0.0.0.0", "--port", "5003", "--write-ahead-log", "/tmp/faucet-wal"]