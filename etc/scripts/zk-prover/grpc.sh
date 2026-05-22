#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  grpc.sh list [target]
  grpc.sh describe [target]
  grpc.sh prove <start_block> [number_of_blocks] [target] [proof_type]
  grpc.sh get <session_id> [target] [receipt_type]
  grpc.sh list-proofs [target] [limit] [offset] [status_filter]

Targets:
  devnet   -> ZK_PROVER_DEVNET_ENDPOINT, defaults to localhost:9000 with -plaintext
  zeronet  -> ZK_PROVER_ZERONET_ENDPOINT
  sepolia  -> ZK_PROVER_SEPOLIA_ENDPOINT
  mainnet  -> ZK_PROVER_MAINNET_ENDPOINT

Any other target value is treated as a literal grpc endpoint.
EOF
}

require_grpcurl() {
  command -v grpcurl >/dev/null 2>&1 || {
    echo "grpcurl is required. Install it with: brew install grpcurl" >&2
    exit 1
  }
}

resolve_endpoint() {
  case "$1" in
    devnet) echo "${ZK_PROVER_DEVNET_ENDPOINT:-localhost:9000}" ;;
    zeronet) : "${ZK_PROVER_ZERONET_ENDPOINT:?set ZK_PROVER_ZERONET_ENDPOINT}" && echo "$ZK_PROVER_ZERONET_ENDPOINT" ;;
    sepolia) : "${ZK_PROVER_SEPOLIA_ENDPOINT:?set ZK_PROVER_SEPOLIA_ENDPOINT}" && echo "$ZK_PROVER_SEPOLIA_ENDPOINT" ;;
    mainnet) : "${ZK_PROVER_MAINNET_ENDPOINT:?set ZK_PROVER_MAINNET_ENDPOINT}" && echo "$ZK_PROVER_MAINNET_ENDPOINT" ;;
    *) echo "$1" ;;
  esac
}

resolve_flags() {
  local target="$1"
  local endpoint="$2"

  case "$target" in
    devnet)
      if [ -n "${ZK_PROVER_DEVNET_GRPCURL_FLAGS+x}" ]; then
        echo "${ZK_PROVER_DEVNET_GRPCURL_FLAGS}"
        return
      fi

      case "$endpoint" in
        localhost:* | 127.*) echo "--plaintext" ;;
        *) echo "${ZK_PROVER_GRPCURL_FLAGS:-}" ;;
      esac
      ;;
    zeronet) echo "${ZK_PROVER_ZERONET_GRPCURL_FLAGS:-}" ;;
    sepolia) echo "${ZK_PROVER_SEPOLIA_GRPCURL_FLAGS:-}" ;;
    mainnet) echo "${ZK_PROVER_MAINNET_GRPCURL_FLAGS:-}" ;;
    *)
      case "$endpoint" in
        localhost:* | 127.*) echo "${ZK_PROVER_GRPCURL_FLAGS:--plaintext}" ;;
        *) echo "${ZK_PROVER_GRPCURL_FLAGS:-}" ;;
      esac
      ;;
  esac
}

run_grpcurl() {
  local target="$1"
  shift

  local endpoint flags
  endpoint="$(resolve_endpoint "$target")"
  flags="$(resolve_flags "$target" "$endpoint")"

  # Intentionally allow grpcurl flags to split so callers can pass multiple flags
  # through ZK_PROVER_*_GRPCURL_FLAGS.
  grpcurl ${flags} "$endpoint" "$@"
}

run_grpcurl_with_data() {
  local target="$1"
  local payload="$2"
  shift 2

  local endpoint flags
  endpoint="$(resolve_endpoint "$target")"
  flags="$(resolve_flags "$target" "$endpoint")"

  # Intentionally allow grpcurl flags to split so callers can pass multiple flags
  # through ZK_PROVER_*_GRPCURL_FLAGS.
  grpcurl ${flags} -d "$payload" "$endpoint" "$@"
}

json_payload() {
  python3 - "$@" <<'PY'
import json
import sys

payload = {}
for arg in sys.argv[1:]:
    key, raw_value = arg.split("=", 1)
    if raw_value.isdigit():
        payload[key] = int(raw_value)
    elif raw_value:
        payload[key] = raw_value

print(json.dumps(payload, separators=(",", ":")))
PY
}

main() {
  require_grpcurl

  local command="${1:-}"
  if [ -z "$command" ]; then
    usage >&2
    exit 1
  fi
  shift

  case "$command" in
    list)
      local target="${1:-devnet}"
      run_grpcurl "$target" list
      ;;
    describe)
      local target="${1:-devnet}"
      run_grpcurl "$target" describe prover.ProverService
      ;;
    prove)
      local start_block="${1:?start_block is required}"
      local number_of_blocks="${2:-1}"
      local target="${3:-devnet}"
      local proof_type="${4:-PROOF_TYPE_COMPRESSED}"
      local payload
      payload="$(json_payload \
        "startBlockNumber=$start_block" \
        "numberOfBlocksToProve=$number_of_blocks" \
        "proofType=$proof_type")"
      run_grpcurl_with_data "$target" "$payload" prover.ProverService/ProveBlock
      ;;
    get)
      local session_id="${1:?session_id is required}"
      local target="${2:-devnet}"
      local receipt_type="${3:-}"
      local payload_args=("sessionId=$session_id")
      if [ -n "$receipt_type" ]; then
        payload_args+=("receiptType=$receipt_type")
      fi
      local payload
      payload="$(json_payload "${payload_args[@]}")"
      run_grpcurl_with_data "$target" "$payload" prover.ProverService/GetProof
      ;;
    list-proofs)
      local target="${1:-devnet}"
      local limit="${2:-20}"
      local offset="${3:-0}"
      local status_filter="${4:-}"
      local payload_args=("limit=$limit" "offset=$offset")
      if [ -n "$status_filter" ]; then
        payload_args+=("statusFilter=$status_filter")
      fi
      local payload
      payload="$(json_payload "${payload_args[@]}")"
      run_grpcurl_with_data "$target" "$payload" prover.ProverService/ListProofs
      ;;
    -h | --help | help)
      usage
      ;;
    *)
      echo "unknown command: $command" >&2
      usage >&2
      exit 1
      ;;
  esac
}

main "$@"
