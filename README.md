# MySocial Faucet

A token faucet service for the MySocial blockchain, allowing developers to request test (tMySo) tokens for development and testing purposes.

## Overview

The MySocial faucet is a web service that dispenses test tokens to requested addresses. It includes rate limiting, metrics, and configurable token distribution settings.

## Features

- REST API for token requests
- Rate limiting to prevent abuse
- Prometheus metrics for monitoring
- Batch request support
- Support for Discord bot integration
- Cloudflare Turnstile support for web requests
- Write-ahead logging for reliability

## API Endpoints

- `POST /gas` - Basic request for gas tokens
- `POST /v1/gas` - Batch request for gas tokens
- `POST /v1/faucet_web_gas` - Web interface request for gas tokens (with Turnstile verification)
- `POST /v1/faucet_discord` - Discord bot request for gas tokens
- `GET /v1/status/:task_id` - Check status of a batch request
- `GET /health` - Health check endpoint

## Usage

### Request Tokens

To request tokens, send a POST request to the faucet endpoint:

```bash
# Example using curl
curl -X POST http://localhost:5003/gas \
  -H "Content-Type: application/json" \
  -d '{"FixedAmountRequest":{"recipient":"0x123..."}}'
```

### Batch Request

```bash
# Example batch request
curl -X POST http://localhost:5003/v1/gas \
  -H "Content-Type: application/json" \
  -d '{"FixedAmountRequest":{"recipient":"0x123..."}}'
```

### Check Request Status

```bash
# Check status of a batch request
curl http://localhost:5003/v1/status/00000000-0000-0000-0000-000000000000
```

## Configuration

```bash
# Basic configuration
cargo run --bin mys-faucet -- \
  --port 5003 \
  --host-ip 127.0.0.1 \
  --amount 1000000000 \
  --num-coins 1 \
  --write-ahead-log /path/to/wal

# Advanced rate limiting
cargo run --bin mys-faucet -- \
  --max-request-per-second 10 \
  --request-buffer-size 10 \
  --max-request-queue-length 10000

# Enable batch mode
cargo run --bin mys-faucet -- \
  --batch-enabled \
  --batch-request-size 500

# Enable authenticated mode (for testnet)
cargo run --bin mys-faucet -- \
  --authenticated \
  --max-requests-per-ip 3
```

### Environment Variables

- `FAUCET_WEB_APP_URL` - URL for the faucet web interface (default: https://faucet.mysocial.network)
- `CLOUDFLARE_TURNSTILE_URL` - URL for Cloudflare Turnstile verification
- `TURNSTILE_SECRET_KEY` - Secret key for Cloudflare Turnstile
- `DISCORD_BOT_PWD` - Password for Discord bot authentication
- `MAX_CONCURRENCY` - Maximum concurrent requests (default: 30)

## Metrics

Prometheus metrics are exposed on port 9184 by default.

## Building

Build the faucet with Cargo:

```bash
cargo build --bin mys-faucet
```

## Running

Run the faucet service:

```bash
cargo run --bin mys-faucet -- [OPTIONS]
```

Use `--help` to see all available configuration options.

## License

Apache-2.0 