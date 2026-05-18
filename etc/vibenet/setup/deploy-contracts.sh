#!/bin/bash
# Vibenet post-setup: deploys demo contracts onto the L2 once base-client is
# reachable, then writes their addresses to /shared/contracts.json for the
# UI.
#
# Inputs (env):
#   L2_RPC_URL            JSON-RPC endpoint of the vibenet L2 client.
#   L2_CHAIN_ID           Numeric L2 chain id.
#   FAUCET_ADDR           Deployer EOA (must be the prefunded faucet).
#   FAUCET_PRIVATE_KEY    Private key for FAUCET_ADDR.
#   OUT_FILE              Where to write contracts.json. Default /shared/contracts.json.
#   VIBENET_BRANCH        Branch name, written into contracts.json for dev visibility.
#   VIBENET_COMMIT        Commit sha, written into contracts.json.
#
# The contract list comes from /setup/contracts.yaml.

set -euo pipefail

OUT_FILE="${OUT_FILE:-/shared/contracts.json}"
CONTRACTS_YAML="${CONTRACTS_YAML:-/setup/contracts.yaml}"
FORGE_ROOT="${FORGE_ROOT:-/setup/contracts}"

: "${L2_RPC_URL:?L2_RPC_URL required}"
: "${L2_CHAIN_ID:?L2_CHAIN_ID required}"
: "${FAUCET_PRIVATE_KEY:?FAUCET_PRIVATE_KEY required}"
: "${FAUCET_ADDR:?FAUCET_ADDR required}"

echo "=== vibenet-setup: waiting for L2 RPC at $L2_RPC_URL ==="
for i in $(seq 1 120); do
  if curl -sf --max-time 2 -X POST -H 'Content-Type: application/json' \
      --data '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}' \
      "$L2_RPC_URL" | jq -e '.result' >/dev/null 2>&1; then
    echo "L2 RPC ready"
    break
  fi
  sleep 1
  if [ "$i" = 120 ]; then echo "ERROR: L2 RPC never came up"; exit 1; fi
done

# Sweep the standard anvil EOAs on L2 into the faucet. op-deployer prefunds
# them via fundDevAccounts so vibenet inherits big balances; vibenet makes
# that liquidity available to users by consolidating it into the faucet.
ANVIL_KEYS=(
  "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
  "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
  "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a"
  "0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6"
  "0x47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a"
  "0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba"
  "0x92db14e403b83dfe3df233f83dfa3a0d7096f21ca9b0d6d6b8d88b2b4ec1564e"
  "0x4bbbf85ce3377467afe5d46f804f221813b2bb87f24d81f60f1fcdbf7cbf4356"
  "0xdbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97"
  "0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6"
)

echo ""
echo "=== sweeping residual anvil balances to faucet ==="
# Anvil pre-funds each account with 10_000 ETH = 1e22 wei which overflows
# 64-bit bash arithmetic. Delegate the math to bc.
for key in "${ANVIL_KEYS[@]}"; do
  addr=$(cast wallet address --private-key "$key")
  bal=$(cast balance "$addr" --rpc-url "$L2_RPC_URL")
  if [ "$bal" = "0" ]; then
    continue
  fi
  gas_price=$(cast gas-price --rpc-url "$L2_RPC_URL")
  # Pad the reserve generously (10x 21k) so EIP-1559 priority tips don't drop
  # us below zero.
  reserve=$(echo "$gas_price * 21000 * 10" | bc)
  send=$(echo "$bal - $reserve" | bc)
  if [ "$(echo "$send <= 0" | bc)" = "1" ]; then
    continue
  fi
  echo "sweep $addr -> $FAUCET_ADDR ($send wei)"
  cast send --rpc-url "$L2_RPC_URL" --private-key "$key" \
    --value "$send" "$FAUCET_ADDR" >/dev/null \
    || echo "  (sweep failed, continuing)"
done

echo ""
echo "=== building contracts ==="
cd "$FORGE_ROOT"
forge build --silent

echo ""
echo "=== deploying contracts from $CONTRACTS_YAML ==="
mkdir -p "$(dirname "$OUT_FILE")"
# Start the output with metadata so the UI always has something to show even
# if deploys fail partway through.
echo "{\"_branch\":\"${VIBENET_BRANCH:-unknown}\",\"_commit\":\"${VIBENET_COMMIT:-unknown}\",\"faucetAddress\":\"${FAUCET_ADDR}\"}" \
  | jq '.' > "$OUT_FILE"

count=$(yq '.contracts | length' "$CONTRACTS_YAML")
for i in $(seq 0 $((count - 1))); do
  name=$(yq ".contracts[$i].name" "$CONTRACTS_YAML")
  artifact=$(yq ".contracts[$i].artifact" "$CONTRACTS_YAML")
  args_json=$(yq -o=json ".contracts[$i].args // []" "$CONTRACTS_YAML")

  # Resolve {{ otherContract }} templates from already-deployed addresses.
  resolved_args=()
  while IFS= read -r a; do
    if [[ "$a" =~ ^\{\{[[:space:]]*(.+)[[:space:]]*\}\}$ ]]; then
      ref="${BASH_REMATCH[1]}"
      addr=$(jq -r --arg k "$ref" '.[$k] // empty' "$OUT_FILE")
      if [ -z "$addr" ]; then
        echo "ERROR: $name references {{ $ref }} but it hasn't been deployed yet"
        exit 1
      fi
      resolved_args+=("$addr")
    else
      resolved_args+=("$a")
    fi
  done < <(echo "$args_json" | jq -r '.[]')

  echo "-> deploying $name ($artifact) with args [${resolved_args[*]:-}]"
  # forge create sometimes reads a stale pending nonce when deploys run back
  # to back. Pin the nonce explicitly using the faucet's current tx count.
  nonce_hex=$(cast rpc --rpc-url "$L2_RPC_URL" eth_getTransactionCount "$FAUCET_ADDR" pending | tr -d '"')
  nonce=$((nonce_hex))
  # Retry on transient "nonce too low" / "already known" races.
  for attempt in 1 2 3 4 5; do
    if out=$(forge create "$artifact" \
        --rpc-url "$L2_RPC_URL" \
        --private-key "$FAUCET_PRIVATE_KEY" \
        --nonce "$nonce" \
        --broadcast \
        --json \
        ${resolved_args[@]:+--constructor-args "${resolved_args[@]}"} 2>&1); then
      break
    fi
    echo "   deploy attempt $attempt failed, retrying: ${out##*: }"
    sleep 2
    nonce_hex=$(cast rpc --rpc-url "$L2_RPC_URL" eth_getTransactionCount "$FAUCET_ADDR" pending | tr -d '"')
    nonce=$((nonce_hex))
    if [ "$attempt" = 5 ]; then
      echo "ERROR: could not deploy $name: $out"
      exit 1
    fi
  done
  addr=$(echo "$out" | jq -r '.deployedTo')
  echo "   $name = $addr"

  TMP=$(mktemp)
  jq --arg k "$name" --arg v "$addr" '. + {($k): $v}' "$OUT_FILE" > "$TMP"
  mv "$TMP" "$OUT_FILE"
done

echo ""
echo "=== contracts.json ==="
cat "$OUT_FILE"
# nginx runs as the nginx user and needs read access. The shared volume is
# owned by root with the restrictive default umask of the foundry image, so
# loosen perms explicitly here.
chmod 0644 "$OUT_FILE"
echo ""
echo "vibenet-setup: done"
