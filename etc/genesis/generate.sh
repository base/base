#!/usr/bin/env bash
# Orchestrate Forge exports; Rust only prepares inputs, hashes state, and assembles files.
set -euo pipefail
# Compose represents unscheduled upgrades as empty environment values. Clap's
# optional integer arguments require those values to be absent instead.
for variable in L2_ISTHMUS_BLOCK L2_BASE_AZUL_BLOCK L2_BASE_BERYL_BLOCK \
  L2_BASE_COBALT_BLOCK L2_BASE_DENIM_BLOCK L2_BASE_ZENITH_BLOCK; do
  if [[ -z "${!variable:-}" ]]; then
    unset "$variable"
  fi
done
base="${BASE_GENESIS_BIN:-base}"
artifacts="${BASE_DEVNET_ARTIFACTS:-build/genesis}"
output="${OUTPUT_DIR-.devnet/genesis}"
args=()
while (($#)); do
  case "$1" in
    --output-dir) output="${2:?missing output directory}"; shift ;;
    --output-dir=*) output="${1#*=}" ;;
    --artifacts-dir) artifacts="${2:?missing artifacts directory}"; shift ;;
    --artifacts-dir=*) artifacts="${1#*=}" ;;
    --stage|--stage=*|--work-dir|--work-dir=*) echo 'Stage arguments are reserved for the workflow' >&2; exit 1 ;;
    --help|-h) exec "$base" genesis --help ;;
    *) args+=("$1") ;;
  esac
  shift
done
mkdir -p "$output"
output="$(cd "$output" && pwd -P)"
exec 9>"$output/.genesis.lock"
flock -n 9 || { echo "Another genesis workflow is using $output" >&2; exit 1; }
project="$(cd "$artifacts/contracts" && pwd)"
mkdir -p "$project/deployments"
work="$(mktemp -d "$project/deployments/genesis-XXXXXXXX")"
trap 'rm -rf "$work"' EXIT
"$base" genesis "${args[@]}" --artifacts-dir "$artifacts" --output-dir "$output" --work-dir "$work" --stage prepare
if [[ "$(jq -r .complete "$work/plan.json")" == true ]]; then
  echo "Reusing complete genesis: $output"
  exit 0
fi
expected="$(jq -er .forge_version "$artifacts/manifest.json")"
[[ "$(forge --version)" == "$expected" ]] || { echo 'Forge version differs from the prepared bundle; run just build genesis-contracts' >&2; exit 1; }
export XDG_DATA_HOME="$project/.tools"
export FOUNDRY_PROFILE=default
run_forge() {
  local adapter="$1" directory="$2" chain="$3"
  mkdir -p "$work/$directory"
  if ! (cd "$project" && forge script "scripts/Genesis.s.sol:$adapter" \
    --sig 'generate(string,string)' "$work/input.json" "$work/$directory" \
    --offline --disable-code-size-limit --chain "$chain" --block-timestamp 0 \
    --sender 0x0000000000000000000000000000000000000001) >"$work/forge.log" 2>&1; then
    cat "$work/forge.log" >&2
    exit 1
  fi
}
l1="$(jq -er .config.l1_chain_id "$work/plan.json")"
l2="$(jq -er .config.l2_chain_id "$work/plan.json")"
run_forge BaseL1Genesis l1-preview "$l1"
run_forge BaseL2Genesis l2 "$l2"
"$base" genesis --work-dir "$work" --stage anchor
run_forge BaseL1Genesis l1-final "$l1"
"$base" genesis --work-dir "$work" --stage assemble
echo "Generated Base genesis: $output"
