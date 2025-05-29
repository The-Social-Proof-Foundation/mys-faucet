#!/bin/bash

# Create config directory
mkdir -p /app/config

# Check if we have the required environment variables
if [ -z "$WALLET_ADDRESS" ]; then
    echo "❌ Missing required WALLET_ADDRESS environment variable"
    exit 1
fi

# Check that we have either private key OR mnemonic
if [ -z "$WALLET_PRIVATE_KEY" ] && [ -z "$WALLET_MNEMONIC" ]; then
    echo "❌ Must provide either WALLET_PRIVATE_KEY or WALLET_MNEMONIC"
    exit 1
fi

# Set default network if not provided
NETWORK_URL=${NETWORK_URL:-"https://fullnode.testnet.mysocial.network:443"}
NETWORK_ALIAS=${NETWORK_ALIAS:-"production"}

echo "🔧 Setting up wallet configuration..."
echo "📍 Network: $NETWORK_URL"
echo "💳 Address: $WALLET_ADDRESS"

# Test network connectivity
echo "🌐 Testing network connectivity..."
if curl -s --connect-timeout 10 --max-time 20 "$NETWORK_URL" > /dev/null; then
    echo "✅ Network connectivity test passed"
else
    echo "❌ Network connectivity test failed"
    echo "🔍 Trying alternative connection test..."
    
    # Extract hostname from URL for basic connectivity test
    HOSTNAME=$(echo "$NETWORK_URL" | sed 's|https\?://||' | sed 's|:.*||')
    if ping -c 3 "$HOSTNAME" > /dev/null 2>&1; then
        echo "✅ Basic hostname ping successful to $HOSTNAME"
    else
        echo "❌ Cannot reach $HOSTNAME"
        echo "🚨 This may cause connection issues with the MySocial network"
    fi
fi

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

# Create keystore file - MySocial format is just an array of base64-encoded keypairs
if [ ! -z "$WALLET_PRIVATE_KEY" ]; then
    echo "🔑 Using provided private key"
    cat > /app/config/mys.keystore << EOF
[
  "$WALLET_PRIVATE_KEY"
]
EOF
elif [ ! -z "$WALLET_MNEMONIC" ]; then
    echo "🎯 Using mnemonic to derive keypair"
    # For now, we'll require the private key since deriving from mnemonic 
    # requires more complex crypto operations that would need the mys binary
    echo "❌ Mnemonic derivation not yet implemented in this script"
    echo "   Please provide WALLET_PRIVATE_KEY instead"
    exit 1
fi

echo "✅ Created client.yaml"
echo "✅ Created mys.keystore"
echo "🔄 Backup mnemonic: ${WALLET_MNEMONIC:-"(not provided)"}"
echo "🚀 Starting MySocial faucet..."

# Start the faucet
exec ./bin/mys-faucet --write-ahead-log /app/faucet.wal 