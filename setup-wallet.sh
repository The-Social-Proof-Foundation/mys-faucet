#!/bin/bash

# Create config directory
mkdir -p /app/config

# Check if we have the required environment variables
if [ -z "$WALLET_ADDRESS" ] || [ -z "$WALLET_PRIVATE_KEY" ] || [ -z "$WALLET_MNEMONIC" ]; then
    echo "❌ Missing required wallet environment variables:"
    echo "   WALLET_ADDRESS, WALLET_PRIVATE_KEY, WALLET_MNEMONIC"
    exit 1
fi

# Set default network if not provided
NETWORK_URL=${NETWORK_URL:-"https://fullnode.testnet.mysocial.network:443"}
NETWORK_ALIAS=${NETWORK_ALIAS:-"production"}

echo "🔧 Setting up wallet configuration..."
echo "📍 Network: $NETWORK_URL"
echo "💳 Address: $WALLET_ADDRESS"

# Create client.yaml
cat > /app/config/client.yaml << EOF
keystore:
  File: /app/config/mys.keystore
envs:
- alias: $NETWORK_ALIAS
  rpc: $NETWORK_URL
  ws: null
  basic_auth: null
active_env: $NETWORK_ALIAS
active_address: $WALLET_ADDRESS
EOF

# Create keystore file (simplified format)
cat > /app/config/mys.keystore << EOF
[
  {
    "address": "$WALLET_ADDRESS",
    "key_scheme": "ed25519",
    "private_key": "$WALLET_PRIVATE_KEY",
    "mnemonic": "$WALLET_MNEMONIC"
  }
]
EOF

echo "✅ Created client.yaml"
echo "✅ Created mys.keystore"
echo "🚀 Starting MySocial faucet..."

# Start the faucet
exec ./bin/mys-faucet --write-ahead-log /app/faucet.wal 