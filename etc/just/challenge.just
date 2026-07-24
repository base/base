set positional-arguments := true
set working-directory := '../..'

# Show challenge helper commands
default:
    @just --justfile {{ source_file() }} --list --list-prefix '  challenge::'

# Run the ZK fork dispute binary (`base-zk-fork-dispute`).
#
# Requires a running prover-service JSON-RPC endpoint (same stack as base-challenger /
# snark-e2e). This recipe starts an Anvil L1 fork if needed; it does not start the prover.
#
# Required env (recipe / anvil setup):
#   BASE_ZK_FORK_PRIVATE_KEY
#   BASE_ZK_FORK_L1_FORK_URL
#   BASE_ZK_FORK_L2_RPC_URL
#   BASE_ZK_FORK_DISPUTE_GAME_FACTORY
#
# Optional env:
#   BASE_ZK_FORK_L1_RPC_URL             # default http://127.0.0.1:18545
#   BASE_ZK_FORK_PROVER_SERVICE_URL     # default http://localhost:9000
#   BASE_ZK_FORK_GAME_ADDRESS
#
# Use an empty game arg when passing game_index:
#   just challenge zk-fork-dispute challenge '' 123
zk-fork-dispute intent='' game='' game_index='' invalid_index='':
    #!/usr/bin/env bash
    set -euo pipefail

    anvil_pid=''
    cleanup() {
      if [[ -n "$anvil_pid" ]]; then
        kill "$anvil_pid" >/dev/null 2>&1 || true
      fi
    }
    trap cleanup EXIT

    : "${BASE_ZK_FORK_PRIVATE_KEY:?set BASE_ZK_FORK_PRIVATE_KEY}"
    : "${BASE_ZK_FORK_L1_FORK_URL:?set BASE_ZK_FORK_L1_FORK_URL}"
    : "${BASE_ZK_FORK_L2_RPC_URL:?set BASE_ZK_FORK_L2_RPC_URL}"
    : "${BASE_ZK_FORK_DISPUTE_GAME_FACTORY:?set BASE_ZK_FORK_DISPUTE_GAME_FACTORY}"
    : "${BASE_ZK_FORK_L1_RPC_URL:=http://127.0.0.1:18545}"
    : "${BASE_ZK_FORK_PROVER_SERVICE_URL:=http://localhost:9000}"

    game_address="${BASE_ZK_FORK_GAME_ADDRESS:-{{game}}}"
    factory="$BASE_ZK_FORK_DISPUTE_GAME_FACTORY"
    default_l1_rpc='http://127.0.0.1:18545'

    if ! curl -fsS -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' "$BASE_ZK_FORK_L1_RPC_URL" >/dev/null 2>&1; then
      if [[ "$BASE_ZK_FORK_L1_RPC_URL" != "$default_l1_rpc" ]]; then
        echo "no RPC at $BASE_ZK_FORK_L1_RPC_URL; start Anvil yourself or use $default_l1_rpc" >&2
        exit 1
      fi
      command -v anvil >/dev/null || { echo "anvil is required to start the L1 fork" >&2; exit 1; }
      anvil_log="$(mktemp -t zk-fork-anvil.XXXXXX.log)"
      anvil --fork-url "$BASE_ZK_FORK_L1_FORK_URL" --host 127.0.0.1 --port 18545 >"$anvil_log" 2>&1 &
      anvil_pid="$!"
      for _ in {1..60}; do
        if curl -fsS -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' "$BASE_ZK_FORK_L1_RPC_URL" >/dev/null 2>&1; then
          break
        fi
        sleep 1
      done
      curl -fsS -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' "$BASE_ZK_FORK_L1_RPC_URL" >/dev/null 2>&1 || {
        echo "anvil did not start; log: $anvil_log" >&2
        exit 1
      }
    fi

    prover_endpoint="${BASE_ZK_FORK_PROVER_SERVICE_URL#http://}"
    prover_endpoint="${prover_endpoint#https://}"
    prover_endpoint="${prover_endpoint%%/*}"
    prover_host="${prover_endpoint%%:*}"
    prover_port="${prover_endpoint##*:}"
    if [[ "$prover_port" == "$prover_host" ]]; then
      case "$BASE_ZK_FORK_PROVER_SERVICE_URL" in
        https://*) prover_port=443 ;;
        *) prover_port=80 ;;
      esac
    fi
    if ! { : >"/dev/tcp/$prover_host/$prover_port"; } >/dev/null 2>&1; then
      echo "prover-service is not reachable at $BASE_ZK_FORK_PROVER_SERVICE_URL" >&2
      echo "start prover-service (+ zk-host worker) before running this recipe" >&2
      exit 1
    fi

    # Private key stays in the environment (BASE_ZK_FORK_PRIVATE_KEY) so it is
    # not visible on the cargo/process argv for the lifetime of proving.
    args=(
      --l1-rpc-url "$BASE_ZK_FORK_L1_RPC_URL"
      --l2-rpc-url "$BASE_ZK_FORK_L2_RPC_URL"
      --prover-service-url "$BASE_ZK_FORK_PROVER_SERVICE_URL"
      --dispute-game-factory "$factory"
    )
    if [[ -n "$game_address" ]]; then
      args+=(--game-address "$game_address")
    fi
    if [[ -n "{{intent}}" ]]; then
      args+=(--dispute-intent "{{intent}}")
    fi
    if [[ -n "{{game_index}}" ]]; then
      args+=(--game-index "{{game_index}}")
    fi
    if [[ -n "{{invalid_index}}" ]]; then
      args+=(--invalid-index "{{invalid_index}}")
    fi

    cargo run -p base-zk-fork-dispute-bin --bin base-zk-fork-dispute -- "${args[@]}"
