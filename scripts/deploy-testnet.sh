#!/usr/bin/env bash
# Builds both contracts, deploys them to Stellar testnet, initializes each
# with the deploying identity as admin, and writes the resulting contract
# IDs to deployments.json at the repo root.
#
# Requires the `stellar` CLI (https://developers.stellar.org/docs/tools/cli)
# and a funded testnet identity. Run from the repo root:
#
#   STELLAR_SOURCE_ACCOUNT=my-testnet-identity ./scripts/deploy-testnet.sh

set -euo pipefail

NETWORK="testnet"
SOURCE_ACCOUNT="${STELLAR_SOURCE_ACCOUNT:?Set STELLAR_SOURCE_ACCOUNT to a funded testnet identity name}"
WASM_DIR="target/wasm32v1-none/release"
DEPLOYMENTS_FILE="deployments.json"

# Fail fast, before anything is built or deployed, if the tools this script
# depends on aren't there — otherwise a missing target can be discovered
# only after ngo-registry has already deployed, leaving a half-finished
# run behind.
check_prerequisites() {
  if ! command -v stellar >/dev/null 2>&1; then
    echo "Error: the 'stellar' CLI is not installed or not on PATH." >&2
    echo "See https://developers.stellar.org/docs/tools/cli for install instructions." >&2
    exit 1
  fi

  if ! command -v rustup >/dev/null 2>&1; then
    echo "Error: 'rustup' is not installed or not on PATH; can't verify the wasm32v1-none target." >&2
    exit 1
  fi

  if ! rustup target list --installed | grep -qx "wasm32v1-none"; then
    echo "Error: the wasm32v1-none target is not installed." >&2
    echo "Install it with: rustup target add wasm32v1-none" >&2
    exit 1
  fi
}

check_prerequisites

# The admin is deliberately separate from the account paying the deploy
# fees. The admin must be a wallet a human can actually sign with, since
# approve_ngo and friends are driven from the browser admin panel — a
# CLI-only deployer key cannot do that. Set STELLAR_ADMIN_ADDRESS to that
# wallet; it falls back to the deployer for a throwaway local deploy.
#
# Resolved to a G-address up front either way: the CLI takes an identity
# name for --source, but an Address-typed *argument* like init's --admin is
# not guaranteed to resolve the same way, and init can only ever be called
# once per contract — so pass something unambiguous.
ADMIN_ADDRESS="${STELLAR_ADMIN_ADDRESS:-$(stellar keys address "$SOURCE_ACCOUNT")}"
echo "Admin address: $ADMIN_ADDRESS"

echo "Building contracts (release, wasm32v1-none)..."
cargo build --workspace --target wasm32v1-none --release

deploy_contract() {
  local wasm_name="$1"
  stellar contract deploy \
    --wasm "$WASM_DIR/${wasm_name}.wasm" \
    --source "$SOURCE_ACCOUNT" \
    --network "$NETWORK"
}

echo "Deploying ngo-registry..."
NGO_REGISTRY_ID=$(deploy_contract "ngo_registry")
echo "  -> $NGO_REGISTRY_ID"

echo "Deploying donation-vault..."
DONATION_VAULT_ID=$(deploy_contract "donation_vault")
echo "  -> $DONATION_VAULT_ID"

echo "Initializing ngo-registry (admin: $ADMIN_ADDRESS)..."
stellar contract invoke \
  --id "$NGO_REGISTRY_ID" \
  --source "$SOURCE_ACCOUNT" \
  --network "$NETWORK" \
  -- init --admin "$ADMIN_ADDRESS"

echo "Initializing donation-vault (admin: $ADMIN_ADDRESS)..."
stellar contract invoke \
  --id "$DONATION_VAULT_ID" \
  --source "$SOURCE_ACCOUNT" \
  --network "$NETWORK" \
  -- init --admin "$ADMIN_ADDRESS"

# deploy-mainnet.sh records its deployment under a "mainnet" key in this same
# file; rewriting the file must not lose it, so carry it over if present.
MAINNET_ENTRY=""
if [ -f "$DEPLOYMENTS_FILE" ] && command -v node >/dev/null 2>&1; then
  MAINNET_ENTRY=$(DEPLOYMENTS_FILE="$DEPLOYMENTS_FILE" node -e '
    const data = JSON.parse(require("fs").readFileSync(process.env.DEPLOYMENTS_FILE, "utf8"));
    if (data.mainnet) process.stdout.write(JSON.stringify(data.mainnet));
  ')
fi

cat > "$DEPLOYMENTS_FILE" <<EOF
{
  "network": "$NETWORK",
  "deployed_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "admin": "$ADMIN_ADDRESS",
  "contracts": {
    "ngo-registry": "$NGO_REGISTRY_ID",
    "donation-vault": "$DONATION_VAULT_ID"
  }
}
EOF

if [ -n "$MAINNET_ENTRY" ]; then
  DEPLOYMENTS_FILE="$DEPLOYMENTS_FILE" MAINNET_ENTRY="$MAINNET_ENTRY" node -e '
    const fs = require("fs");
    const data = JSON.parse(fs.readFileSync(process.env.DEPLOYMENTS_FILE, "utf8"));
    data.mainnet = JSON.parse(process.env.MAINNET_ENTRY);
    fs.writeFileSync(process.env.DEPLOYMENTS_FILE, JSON.stringify(data, null, 2) + "\n");
  '
fi

echo "Wrote $DEPLOYMENTS_FILE"
