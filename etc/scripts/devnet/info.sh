#!/usr/bin/env bash
# Writes a JSON manifest of the running Docker devnet and prints it to stdout.
# Bump MANIFEST_VERSION when renaming or removing a field.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

OUT="${1:-.devnet/manifest.json}"
L1_RPC="${2:-$L1_RPC_URL}"
CONFIGS=".devnet/l2/configs"
MANIFEST_VERSION=1

for file in rollup.json l1-addresses.json; do
    [ -f "$CONFIGS/$file" ] || {
        echo "ERROR: $CONFIGS/$file not found; start the devnet with 'just devnet up' or 'just devnet up-single'" >&2
        exit 1
    }
done

# container key URL; only containers that are running appear in the manifest.
ENDPOINTS="
l1-el rpc $L1_RPC_URL
l1-el ws $L1_WS_URL
l1-cl beacon $L1_BEACON_URL
base-builder rpc $L2_BUILDER_RPC_URL
base-builder ws $L2_BUILDER_WS_URL
base-builder consensusRpc $L2_BUILDER_OP_RPC_URL
base-builder flashblocksWs ws://localhost:$L2_BUILDER_FLASHBLOCKS_PORT
base-sequencer-1 rpc http://localhost:$L2_SEQ1_HTTP_PORT
base-sequencer-1 ws ws://localhost:$L2_SEQ1_WS_PORT
base-sequencer-1 consensusRpc http://localhost:$L2_SEQ1_CL_RPC_PORT
base-sequencer-1 flashblocksWs ws://localhost:$L2_SEQ1_FLASHBLOCKS_PORT
base-sequencer-2 rpc http://localhost:$L2_SEQ2_HTTP_PORT
base-sequencer-2 ws ws://localhost:$L2_SEQ2_WS_PORT
base-sequencer-2 consensusRpc http://localhost:$L2_SEQ2_CL_RPC_PORT
base-sequencer-2 flashblocksWs ws://localhost:$L2_SEQ2_FLASHBLOCKS_PORT
base-client rpc $L2_CLIENT_RPC_URL
base-client ws $L2_CLIENT_WS_URL
base-client consensusRpc $L2_CLIENT_OP_RPC_URL
base-rpc rpc $L2_BASE_RPC_URL
base-rpc ws $L2_BASE_RPC_WS_URL
base-rpc consensusRpc $L2_BASE_RPC_OP_RPC_URL
base-shadow-validator rpc http://localhost:$L2_SHADOW_HTTP_PORT
base-shadow-validator ws ws://localhost:$L2_SHADOW_WS_PORT
base-shadow-validator consensusRpc http://localhost:$L2_SHADOW_CL_RPC_PORT
base-proofs rpc $L2_PROOFS_RPC_URL
base-proofs-consensus consensusRpc $L2_PROOFS_OP_RPC_URL
op-conductor-0 rpc $CONDUCTOR0_RPC_URL
op-conductor-1 rpc $CONDUCTOR1_RPC_URL
op-conductor-2 rpc $CONDUCTOR2_RPC_URL
proxyd rpc $L2_PROXYD_RPC_URL
"
RUNNING=$(docker ps --format '{{.Names}}' | jq -R . | jq -s .)
SERVICES=$(jq -Rn --argjson running "$RUNNING" '
    [inputs | select(. != "") | split(" ") | select(.[0] as $svc | $running | index($svc))]
    | reduce .[] as [$svc, $key, $url] ({}; .[$svc][$key] = $url)' <<<"$ENDPOINTS")
[ "$SERVICES" != "{}" ] || {
    echo "ERROR: no devnet containers are running" >&2
    exit 1
}

PREDEPLOYS=$(jq -n '{
    WETH9: "0x4200000000000000000000000000000000000006",
    L2CrossDomainMessenger: "0x4200000000000000000000000000000000000007",
    L2StandardBridge: "0x4200000000000000000000000000000000000010",
    SequencerFeeVault: "0x4200000000000000000000000000000000000011",
    OptimismMintableERC20Factory: "0x4200000000000000000000000000000000000012",
    GasPriceOracle: "0x420000000000000000000000000000000000000F",
    L1Block: "0x4200000000000000000000000000000000000015",
    L2ToL1MessagePasser: "0x4200000000000000000000000000000000000016",
    L2ERC721Bridge: "0x4200000000000000000000000000000000000014",
    OptimismMintableERC721Factory: "0x4200000000000000000000000000000000000017",
    ProxyAdmin: "0x4200000000000000000000000000000000000018",
    BaseFeeVault: "0x4200000000000000000000000000000000000019",
    L1FeeVault: "0x420000000000000000000000000000000000001a",
    OperatorFeeVault: "0x420000000000000000000000000000000000001B",
    SchemaRegistry: "0x4200000000000000000000000000000000000020",
    EAS: "0x4200000000000000000000000000000000000021",
    Multicall3: "0xcA11bde05977b3631167028862bE2a173976CA11",
    Create2Deployer: "0x13b0D85CcB8bf860b6b79AF3029fCA081AE9beF2",
    Permit2: "0x000000000022D473030F116dDEE9F6B43aC78BA3"
}')

# role:variable in devnet-env holding the address or private key a devnet service uses.
ROLES=(
    "deployer:DEPLOYER_ADDR"
    "l1-fee-recipient:FEE_RECIPIENT"
    "sequencer:SEQUENCER_ADDR"
    "batcher:BATCHER_ADDR"
    "proposer:PROPOSER_ADDR"
    "challenger:CHALLENGER_ADDR"
    "shadow-batcher:SHADOW_BATCHER_ADDR"
    "builder-p2p:BUILDER_P2P_KEY"
    "sequencer-1-p2p:SEQ1_P2P_KEY"
    "sequencer-2-p2p:SEQ2_P2P_KEY"
)
ROLE_VALUES=$(for entry in "${ROLES[@]}"; do
    var="${entry#*:}"
    printf '%s %s\n' "${entry%%:*}" "${!var:-}"
done)
ACCOUNTS_JSON=$(printf '%s\n' "${ACCOUNTS[@]}" | jq -Rn --arg roles "$ROLE_VALUES" '
    def norm: ascii_downcase | ltrimstr("0x");
    ($roles | split("\n") | map(split(" ") | select(length == 2 and .[1] != ""))) as $roles
    | [inputs | split(":") as [$address, $key] | {
        address: $address,
        privateKey: $key,
        roles: [$roles[] | select((.[1] | norm) as $v | $v == ($address | norm) or $v == ($key | norm)) | .[0]]
    }]')

# Proof-window values are read live from the portal and its anchor state registry; null if unavailable.
uint_call() {
    cast call --rpc-url "$L1_RPC" "$1" "$2" 2>/dev/null | cast to-dec 2>/dev/null || echo null
}
PORTAL=$(jq -r '.OptimismPortalProxy' "$CONFIGS/l1-addresses.json")
ASR=$(cast call --rpc-url "$L1_RPC" "$PORTAL" 'anchorStateRegistry()(address)' 2>/dev/null || true)
PROOF_MATURITY=$(uint_call "$PORTAL" 'proofMaturityDelaySeconds()')
FINALITY_DELAY=null
RESPECTED_GAME_TYPE=null
if [ -n "$ASR" ]; then
    FINALITY_DELAY=$(uint_call "$ASR" 'disputeGameFinalityDelaySeconds()')
    RESPECTED_GAME_TYPE=$(uint_call "$ASR" 'respectedGameType()')
fi

MANIFEST=$(jq -n \
    --argjson version "$MANIFEST_VERSION" \
    --slurpfile rollup "$CONFIGS/rollup.json" \
    --slurpfile l1Contracts "$CONFIGS/l1-addresses.json" \
    --argjson services "$SERVICES" \
    --argjson predeploys "$PREDEPLOYS" \
    --argjson accounts "$ACCOUNTS_JSON" \
    --arg asr "$ASR" \
    --argjson proofMaturity "$PROOF_MATURITY" \
    --argjson finalityDelay "$FINALITY_DELAY" \
    --argjson respectedGameType "$RESPECTED_GAME_TYPE" \
    '{
        version: $version,
        l1ChainId: $rollup[0].l1_chain_id,
        l2ChainId: $rollup[0].l2_chain_id,
        services: $services,
        l1Contracts: $l1Contracts[0],
        l2Predeploys: $predeploys,
        accounts: $accounts,
        proofWindow: {
            anchorStateRegistry: (if $asr == "" then null else $asr end),
            proofMaturityDelaySeconds: $proofMaturity,
            disputeGameFinalityDelaySeconds: $finalityDelay,
            respectedGameType: $respectedGameType
        }
    }')

mkdir -p "$(dirname "$OUT")"
if ! printf '%s\n' "$MANIFEST" 2>/dev/null > "$OUT"; then
    # On Linux, Docker creates .devnet as root, so write through a container like `just devnet down` does.
    out_dir="$(cd "$(dirname "$OUT")" && pwd)"
    docker run --rm -i --quiet -v "$out_dir:/out" alpine sh -c "cat > '/out/$(basename "$OUT")'" <<<"$MANIFEST"
fi
printf '%s\n' "$MANIFEST"
echo "Wrote $OUT" >&2
