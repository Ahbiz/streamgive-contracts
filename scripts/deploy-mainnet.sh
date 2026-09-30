#!/usr/bin/env bash
# Builds both contracts, deploys them to Stellar MAINNET, initializes each
# with the admin wallet, and records the resulting contract IDs under the
# "mainnet" key of deployments.json at the repo root.
#
# Mainnet deploys spend real funds and can't be undone (init can only be
# called once, and ngo-registry has no admin transfer), so this is stricter
# than deploy-testnet.sh:
#
#   * it refuses to run without --confirm;
#   * it always talks to the Public network: the passphrase is pinned here,
#     and a conflicting STELLAR_NETWORK_PASSPHRASE aborts the run;
#   * STELLAR_ADMIN_ADDRESS is mandatory (no fallback to the deployer key)
#     and must be a valid G-address;
#   * it refuses to overwrite an existing "mainnet" entry in deployments.json.
#
# Requires the `stellar` CLI (https://developers.stellar.org/docs/tools/cli),
# `node` (to update deployments.json), and a funded mainnet identity. Run from
# the repo root:
#
#   STELLAR_SOURCE_ACCOUNT=my-mainnet-identity \
#   STELLAR_ADMIN_ADDRESS=G... \
#   STELLAR_RPC_URL=https://<your-mainnet-rpc> \
#     ./scripts/deploy-mainnet.sh --confirm

set -euo pipefail

NETWORK="mainnet"
MAINNET_PASSPHRASE="Public Global Stellar Network ; September 2015"
WASM_DIR="target/wasm32v1-none/release"
DEPLOYMENTS_FILE="deployments.json"

usage() {
  echo "Usage: $0 --confirm" >&2
  echo "  --confirm   acknowledge that this deploys to Stellar MAINNET with real funds" >&2
}

CONFIRMED=0
for arg in "$@"; do
  case "$arg" in
    --confirm) CONFIRMED=1 ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "Error: unknown argument '$arg'." >&2
      usage
      exit 1
      ;;
  esac
done

if [ "$CONFIRMED" -ne 1 ]; then
  echo "Error: refusing to deploy to MAINNET without --confirm." >&2
  usage
  exit 1
fi

SOURCE_ACCOUNT="${STELLAR_SOURCE_ACCOUNT:?Set STELLAR_SOURCE_ACCOUNT to a funded mainnet identity name}"
ADMIN_ADDRESS="${STELLAR_ADMIN_ADDRESS:?Set STELLAR_ADMIN_ADDRESS to the mainnet admin wallet (a G... address)}"
RPC_URL="${STELLAR_RPC_URL:?Set STELLAR_RPC_URL to a mainnet Soroban RPC endpoint}"

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

  if ! command -v node >/dev/null 2>&1; then
    echo "Error: 'node' is not installed or not on PATH; it is needed to update $DEPLOYMENTS_FILE." >&2
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

# The passphrase is what actually distinguishes networks: a transaction
# signed for one passphrase is invalid on another. It is pinned above and
# passed explicitly on every call, so the only way to get it wrong is an
# environment override — reject that rather than silently ignoring it.
check_network_passphrase() {
  local from_env="${STELLAR_NETWORK_PASSPHRASE:-}"
  if [ -n "$from_env" ] && [ "$from_env" != "$MAINNET_PASSPHRASE" ]; then
    echo "Error: STELLAR_NETWORK_PASSPHRASE is set to a non-mainnet value:" >&2
    echo "  $from_env" >&2
    echo "Expected: $MAINNET_PASSPHRASE" >&2
    exit 1
  fi
}

check_admin_address() {
  if ! [[ "$ADMIN_ADDRESS" =~ ^G[A-Z2-7]{55}$ ]]; then
    echo "Error: STELLAR_ADMIN_ADDRESS is not a valid Stellar account address (G...): $ADMIN_ADDRESS" >&2
    exit 1
  fi
}

# Never clobber a recorded mainnet deployment: re-running would deploy fresh
# contracts and orphan the ones users may already be using.
check_no_existing_deployment() {
  [ -f "$DEPLOYMENTS_FILE" ] || return 0

  local status=0
  DEPLOYMENTS_FILE="$DEPLOYMENTS_FILE" node -e '
    const fs = require("fs");
    let data;
    try {
      data = JSON.parse(fs.readFileSync(process.env.DEPLOYMENTS_FILE, "utf8"));
    } catch (e) {
      console.error("Error: could not parse " + process.env.DEPLOYMENTS_FILE + ": " + e.message);
      process.exit(2);
    }
    process.exit(data.mainnet ? 1 : 0);
  ' || status=$?

  if [ "$status" -eq 1 ]; then
    echo "Error: $DEPLOYMENTS_FILE already has a \"mainnet\" entry; refusing to overwrite it." >&2
    echo "If you really mean to redeploy, remove that entry by hand first." >&2
    exit 1
  elif [ "$status" -ne 0 ]; then
    exit 1
  fi
}

check_prerequisites
check_network_passphrase
check_admin_address
check_no_existing_deployment

echo "About to deploy to Stellar MAINNET."
echo "  Deployer identity: $SOURCE_ACCOUNT"
echo "  Admin address:     $ADMIN_ADDRESS"
echo "  RPC URL:           $RPC_URL"

echo "Building contracts (release, wasm32v1-none)..."
cargo build --workspace --target wasm32v1-none --release

NGO_REGISTRY_ID=""
DONATION_VAULT_ID=""

# If anything fails after a contract is already on-chain, print what was
# deployed so it isn't lost — nothing has been written to deployments.json.
report_partial_deploy() {
  echo >&2
  echo "Deploy failed part-way; $DEPLOYMENTS_FILE was NOT updated." >&2
  echo "  ngo-registry:   ${NGO_REGISTRY_ID:-<not deployed>}" >&2
  echo "  donation-vault: ${DONATION_VAULT_ID:-<not deployed>}" >&2
}
trap report_partial_deploy ERR

stellar_network_args=(--rpc-url "$RPC_URL" --network-passphrase "$MAINNET_PASSPHRASE")

deploy_contract() {
  local wasm_name="$1"
  stellar contract deploy \
    --wasm "$WASM_DIR/${wasm_name}.wasm" \
    --source "$SOURCE_ACCOUNT" \
    "${stellar_network_args[@]}"
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
  "${stellar_network_args[@]}" \
  -- init --admin "$ADMIN_ADDRESS"

echo "Initializing donation-vault (admin: $ADMIN_ADDRESS)..."
stellar contract invoke \
  --id "$DONATION_VAULT_ID" \
  --source "$SOURCE_ACCOUNT" \
  "${stellar_network_args[@]}" \
  -- init --admin "$ADMIN_ADDRESS"

# Add the mainnet entry next to whatever is already in the file (the
# testnet deployment lives at the top level), leaving that untouched.
DEPLOYMENTS_FILE="$DEPLOYMENTS_FILE" \
NETWORK="$NETWORK" \
DEPLOYED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
ADMIN_ADDRESS="$ADMIN_ADDRESS" \
NETWORK_PASSPHRASE="$MAINNET_PASSPHRASE" \
NGO_REGISTRY_ID="$NGO_REGISTRY_ID" \
DONATION_VAULT_ID="$DONATION_VAULT_ID" \
  node -e '
    const fs = require("fs");
    const file = process.env.DEPLOYMENTS_FILE;
    const data = fs.existsSync(file) ? JSON.parse(fs.readFileSync(file, "utf8")) : {};
    data.mainnet = {
      network: process.env.NETWORK,
      network_passphrase: process.env.NETWORK_PASSPHRASE,
      deployed_at: process.env.DEPLOYED_AT,
      admin: process.env.ADMIN_ADDRESS,
      contracts: {
        "ngo-registry": process.env.NGO_REGISTRY_ID,
        "donation-vault": process.env.DONATION_VAULT_ID,
      },
    };
    fs.writeFileSync(file, JSON.stringify(data, null, 2) + "\n");
  '

trap - ERR
echo "Wrote mainnet contract IDs to $DEPLOYMENTS_FILE"
