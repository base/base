#!/usr/bin/env bash

set -euo pipefail
set -a
[ ! -f .env ] || source .env
set +a

l1_rpc="${L1_RPC_URL:-https://ethereum-full-sepolia-k8s-dev.cbhq.net}"
l2_rpc="${L2_RPC_URL:-https://base-sepolia-reth-proofs-k8s-donotuse.cbhq.net:8545}"
rollup_rpc="${ROLLUP_RPC_URL:-${L2_OUTPUT_ROOT_RPC_URL:-https://base-sepolia-reth-internal-rpc-donotuse.cbhq.net:7545}}"
l1_beacon="${L1_BEACON_URL:-https://ethereum-full-sepolia-k8s-dev.cbhq.net:5052}"
l2_chain_id=84532
postgres_port=5432
requester_rpc=127.0.0.1:9000
worker_rpc=127.0.0.1:9001
nitro_signer_rpc=127.0.0.1:8000
tdx_signer_rpc=127.0.0.1:8010
proposer_private_key="${BASE_PROPOSER_PRIVATE_KEY:?BASE_PROPOSER_PRIVATE_KEY must be set in .env or the environment}"
forge_account="${TDX_SEPOLIA_FORGE_ACCOUNT:-testnet-admin}"
contracts_dir="$(cd ../contracts && pwd)"
deployments="${TDX_SEPOLIA_DEPLOYMENTS:-$contracts_dir/deployments/11155111-dev-with-tdx.json}"
deploy_config="${TDX_SEPOLIA_DEPLOY_CONFIG:-$contracts_dir/deploy-config/zeronet-tdx.json}"
pg_container=base-prover-service-tdx-sepolia
pg_password=postgres

registry="$(jq -er '.TEEProverRegistry' "$deployments")"
anchor_state_registry="$(jq -er '.AnchorStateRegistry' "$deployments")"
dispute_game_factory="$(jq -er '.DisputeGameFactory' "$deployments")"
game_type="$(jq -r '.multiproofGameType // 621' "$deploy_config")"
nitro_image_hash="$(jq -er '.teeNitroImageHash' "$deploy_config")"
tdx_image_hash="$(jq -er '.teeTdxImageHash' "$deploy_config")"

wait_rpc() {
    local url="$1" name="$2"
    shift 2
    for _ in {1..120}; do
        "$@" --rpc-url "$url" >/dev/null 2>&1 && return 0
        sleep 1
    done
    echo "Timed out waiting for $name at $url" >&2
    exit 1
}

signer_from_rpc() {
    local public_key_body public_key_hash
    public_key_body="$(cast rpc enclave_signerPublicKey --rpc-url "$1" \
        | python3 -c "import json, sys; data = json.load(sys.stdin)[0]; print('0x' + bytes(data[1:]).hex())")"
    public_key_hash="$(cast keccak "$public_key_body")"
    echo "0x${public_key_hash: -40}"
}

echo "Building local offchain binaries"
cargo build -p base-prover-service-bin -p base-prover-tdx -p base-proposer-bin
cargo build -p base-prover-nitro-host --features local,worker
export RUST_LOG="${RUST_LOG:-info}"

trap 'kill $(jobs -p) 2>/dev/null || true; wait 2>/dev/null || true' EXIT INT TERM

docker rm -f "$pg_container" >/dev/null 2>&1 || true
docker run -d \
    --name "$pg_container" \
    -e POSTGRES_USER=postgres \
    -e POSTGRES_PASSWORD="$pg_password" \
    -e POSTGRES_DB=proverdb \
    -p "127.0.0.1:$postgres_port:5432" \
    -v "$PWD/crates/proof/prover-service/db/migrations:/docker-entrypoint-initdb.d:ro" \
    postgres:17-alpine >/dev/null
until docker exec "$pg_container" pg_isready -U postgres -d proverdb >/dev/null 2>&1; do
    sleep 1
done

echo "Starting prover-service"
POSTGRES_HOST=127.0.0.1 \
POSTGRES_PORT="$postgres_port" \
POSTGRES_DB=proverdb \
POSTGRES_USER=postgres \
POSTGRES_PASSWORD="$pg_password" \
POSTGRES_SSLMODE=disable \
    target/debug/base-prover-service \
    --rpc-listen-addr "$requester_rpc" \
    --worker-rpc-listen-addr "$worker_rpc" &
wait_rpc "http://$requester_rpc" prover-service-requester \
    cast rpc prover_listProofs '{"offset":0,"limit":1}'
wait_rpc "http://$worker_rpc" prover-service-worker \
    cast rpc prover_getProofSession '{"session_id":"__ready__","session_type":"stark"}'

echo "Starting Nitro worker"
target/debug/base-prover-nitro-host local \
    --l1-eth-url "$l1_rpc" \
    --l2-eth-url "$l2_rpc" \
    --l1-beacon-url "$l1_beacon" \
    --l2-chain-id "$l2_chain_id" \
    --listen-addr "$nitro_signer_rpc" \
    --prover-service-endpoint "http://$worker_rpc" \
    --enable-experimental-witness-endpoint &
wait_rpc "http://$nitro_signer_rpc" nitro-signer-rpc cast rpc enclave_signerPublicKey

echo "Starting TDX worker"
target/debug/base-prover-tdx local \
    --l1-eth-url "$l1_rpc" \
    --l2-eth-url "$l2_rpc" \
    --l1-beacon-url "$l1_beacon" \
    --l2-chain-id "$l2_chain_id" \
    --listen-addr "$tdx_signer_rpc" \
    --prover-service-endpoint "http://$worker_rpc" \
    --enable-experimental-witness-endpoint &
wait_rpc "http://$tdx_signer_rpc" tdx-signer-rpc cast rpc enclave_signerPublicKey

nitro_signer="$(signer_from_rpc "http://$nitro_signer_rpc")"
tdx_signer="$(signer_from_rpc "http://$tdx_signer_rpc")"

echo "Registering local TEE signers in $registry"
owner="$(cast wallet address --account "$forge_account")"
cast send "$registry" "addDevSigner(address,bytes32,uint8)" "$nitro_signer" "$nitro_image_hash" 1 \
    --rpc-url "$l1_rpc" --account "$forge_account" --from "$owner"
cast send "$registry" "addDevSigner(address,bytes32,uint8)" "$tdx_signer" "$tdx_image_hash" 2 \
    --rpc-url "$l1_rpc" --account "$forge_account" --from "$owner"

echo "Starting proposer"
echo "Proposer address: $(cast wallet address "$proposer_private_key")"
target/debug/base-proposer \
    --prover-rpc "http://$requester_rpc" \
    --l1-eth-rpc "$l1_rpc" \
    --l2-eth-rpc "$l2_rpc" \
    --rollup-rpc "$rollup_rpc" \
    --anchor-state-registry-addr "$anchor_state_registry" \
    --dispute-game-factory-addr "$dispute_game_factory" \
    --game-type "$game_type" \
    --tee-proof-mode both \
    --allow-non-finalized \
    --private-key "$proposer_private_key" \
    --poll-interval "${TDX_SEPOLIA_PROPOSER_POLL_INTERVAL:-12s}"
