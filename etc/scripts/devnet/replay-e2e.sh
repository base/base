#!/usr/bin/env bash
set -euo pipefail

export CARGO_TARGET_DIR="$PWD/target"
for tool in docker just cast jq curl; do
  command -v "$tool" >/dev/null || { echo "ERROR: missing $tool" >&2; exit 1; }
done
docker info >/dev/null
# shellcheck source=etc/docker/devnet-env
source etc/docker/devnet-env
export COMPOSE_PROFILES=replay L2_BASE_DENIM_BLOCK=
export VALIDITY_BUILDER_RPC_URLS="http://base-builder:$L2_BUILDER_HTTP_PORT,http://replay-recorder:${REPLAY_RECORDER_PORT:-8550}"
mkdir -p .devnet
run="$(mktemp -d "$PWD/.devnet/replay-e2e.XXXXXX")"
export DEVNET_ROOT="$run/devnet"
project="replay-e2e-$(basename "$run" | tr '[:upper:].' '[:lower:]-')"
if [[ -n "$(docker ps -aq --filter "label=com.docker.compose.project=$project")" ]]; then
  echo "ERROR: isolated project $project already exists" >&2
  exit 1
fi
subnet="172.28.$((RANDOM % 200 + 20))"
export L2_BOOTNODE_DOCKER_SUBNET="$subnet.0/24"
export L2_EL_BOOTNODE_ADVERTISE_IP="$subnet.41" L2_CL_BOOTNODE_ADVERTISE_IP="$subnet.40"
export L2_BUILDER_ADVERTISE_IP="$subnet.20" L2_CLIENT_ADVERTISE_IP="$subnet.21"
export L2_EL_BOOTNODE_ENODE="enode://$L2_EL_BOOTNODE_ENODE_ID@$subnet.41:$L2_EL_BOOTNODE_P2P_PORT"
base_image="base:$project"
builder_image="base-builder:$project"
docker compose --project-name "$project" --env-file etc/docker/devnet-env \
  -f etc/docker/docker-compose.yml config --format json | jq --arg image "$base_image" '
  .services |= with_entries(select(.key | IN("setup-devnet", "l1-el", "l1-cl", "l1-vc",
    "base-bootnode", "base-builder", "base-client", "replay-recorder"))) |
  .services |= map_values(del(.container_name, .build) | .ports = [] |
    if .image == "base:local" then .image = $image else . end) |
  .services["base-client"].ports = [{target: 8545, published: "0", host_ip: "127.0.0.1"}] |
  .services["base-builder"].ports = [{target: 7545, published: "0", host_ip: "127.0.0.1"}]
' >"$run/compose.json"
compose=(docker compose --project-name "$project" -f "$run/compose.json")
shadow="$project-shadow"
runner="$project-runner"

cleanup() {
  local status=$?
  trap - EXIT
  if (( status != 0 )); then
    "${compose[@]}" logs --no-color --tail 80 >"$run/devnet.log" 2>&1 || true
    docker logs "$shadow" >"$run/shadow.log" 2>&1 || true
    echo "FAILED ($status); artifacts: $run" >&2
  fi
  docker rm -f "$runner" "$shadow" >/dev/null 2>&1 || true
  if ! "${compose[@]}" down --volumes; then
    echo "ERROR: teardown failed for $project" >&2
    status=1
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

base() {
  docker run --rm --name "$runner" --network "${project}_default" \
    -v "$run:/e2e" --entrypoint /usr/bin/timeout "$base_image" 600 /app/base "$@"
}

echo "Building replay CLI, devnet, and shadow builder from this checkout"
just devnet _build-setup-image
PROFILE=dev BASE_SUCCINCT_ELF_REQUIRE=0 docker buildx bake -f etc/docker/docker-bake.hcl \
  base builder --set "base.tags=$base_image" --set "builder.tags=$builder_image" --load
"${compose[@]}" up -d --no-build replay-recorder base-builder base-client l1-vc
client="http://$("${compose[@]}" port base-client "$L2_CLIENT_HTTP_PORT")"
canonical="http://$("${compose[@]}" port base-builder "$L2_BUILDER_HTTP_PORT")"
deadline=$((SECONDS + 180))
until cast block-number --rpc-url "$client" --rpc-timeout 2 >/dev/null 2>&1; do
  (( SECONDS < deadline )) || { echo "ERROR: client RPC startup timed out" >&2; exit 1; }
done

echo "Recording transfers through mempool for 120 seconds"
nonce="$(cast nonce --rpc-url "$canonical" --rpc-timeout 5 "$ANVIL_ACCOUNT_0_ADDR")"
end=$((SECONDS + 120))
while (( SECONDS < end )); do
  cast send --rpc-url "$client" --rpc-timeout 5 --timeout 30 --json \
    --private-key "$ANVIL_ACCOUNT_0_KEY" "$ANVIL_ACCOUNT_1_ADDR" --value 1wei --nonce "$nonce" \
    | tee "$run/receipt.json" | jq -e '.status == "0x1"' >/dev/null
  nonce=$((nonce + 1))
done
jq '{transactionHash, blockHash, blockNumber, status}' "$run/receipt.json"
last="$(cast block-number --rpc-url "$canonical" --rpc-timeout 5)"
(( last >= 2 )) || { echo "ERROR: need at least two canonical blocks" >&2; exit 1; }
# Flush the open recorder minute before export; Compose waits for graceful exit.
"${compose[@]}" stop replay-recorder
base replay export --from 1 --to "$last" --network devnet \
  --rpc-url "http://base-builder:$L2_BUILDER_HTTP_PORT" \
  --store /e2e/devnet/replay --out /e2e/export
jq -e '.record_count > 0' "$run/export/manifest.json" >/dev/null
echo "N=0; replay blocks 1..$last"
jq '{from_block, to_block, record_count}' "$run/export/manifest.json"

clock_flag="$(base replay run --export /e2e/export --print-clock-start)"
epoch="${clock_flag##* }"
[[ "$epoch" =~ ^[0-9]+$ ]] || { echo "ERROR: invalid replay clock epoch" >&2; exit 1; }
# A fresh database with the canonical genesis is exactly state N=0. No live DB copy.
# Timestamp inside the container immediately before exec approximates clock install.
docker run -d --name "$shadow" --network "${project}_default" -v "$run:/e2e" \
  --entrypoint /bin/sh "$builder_image" -ec '
    date +%s%3N > /e2e/builder-started-at-ms
    exec /app/base-builder node --chain /e2e/devnet/l2/configs/genesis.json \
      --datadir /e2e/shadow --disable-discovery --max-outbound-peers 0 \
      --http --http.addr 0.0.0.0 --http.port 9545 \
      --authrpc.addr 0.0.0.0 --authrpc.port 9551 --authrpc.jwtsecret /e2e/devnet/l1/configs/jwt.hex \
      --replay.clock-start-ms "$1" --rollup.chain-block-time 2000 \
      --flashblocks.block-time 200 --builder.payload-builder-cutover
  ' sh "$epoch"
deadline=$((SECONDS + 20))
until docker exec "$shadow" curl -sf --max-time 1 -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' \
  http://localhost:9545 >/dev/null; do
  (( SECONDS < deadline )) || { echo "ERROR: shadow RPC startup timed out" >&2; exit 1; }
done
started="$(<"$run/builder-started-at-ms")"
base replay run --export /e2e/export --chain /e2e/devnet/l2/configs/genesis.json \
  --l2-rpc "http://base-builder:$L2_BUILDER_HTTP_PORT" \
  --builder-engine "http://$shadow:9551" --builder-rpc "http://$shadow:9545" \
  --builder.jwt-secret /e2e/devnet/l1/configs/jwt.hex --builder-started-at-ms "$started" \
  --clock-start-ms "$epoch" --out /e2e/diff
jq -e --argjson last "$last" '
  .schema == "replay-diff/v1" and .parent_block_number == 0 and
  (.blocks | length) == $last and .blocks[0].warmup == true and
  ([.blocks[].block_number] == [range(1; $last + 1)])
' "$run/diff/diff.json" >/dev/null
test -s "$run/diff/diff.md"
cat "$run/diff/diff.md"
echo "PASS: reports in $run/diff; tearing down $project"
