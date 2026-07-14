set positional-arguments := true
set working-directory := '../..'

_requester_endpoint := "${PROVER_SERVICE_REQUESTER_ENDPOINT:-http://localhost:${PROVER_SERVICE_RPC_PORT:-9000}}"

# Show prover-service request commands
default:
    @just --justfile {{ source_file() }} --list --list-prefix '  zk-prover::'

# List JSON-RPC methods exposed by the active prover-service requester endpoint
methods:
    @just --justfile {{ source_file() }} _rpc rpc_methods '[]'

# Alias for listing JSON-RPC methods
list:
    @just --justfile {{ source_file() }} methods

# Request a proof or dry-run for a block range: proof_type=compressed|snark
prove start_block number_of_blocks='1' proof_type='compressed' session_id='' prover_address='' l1_head='':
    #!/usr/bin/env bash
    set -euo pipefail
    command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 1; }
    start_block={{ quote(start_block) }}
    number_of_blocks={{ quote(number_of_blocks) }}
    proof_type={{ quote(proof_type) }}
    proof_type="${proof_type#proof_type=}"
    proof_type="$(printf '%s' "$proof_type" | tr '[:upper:]-' '[:lower:]_')"
    session_id={{ quote(session_id) }}
    prover_address={{ quote(prover_address) }}
    l1_head={{ quote(l1_head) }}

    case "$start_block" in
      ''|*[!0-9]*) echo "start_block must be a non-negative integer" >&2; exit 1 ;;
    esac
    case "$number_of_blocks" in
      ''|*[!0-9]*) echo "number_of_blocks must be a non-negative integer" >&2; exit 1 ;;
    esac
    if [ "$number_of_blocks" -eq 0 ]; then
      echo "number_of_blocks must be greater than zero" >&2
      exit 1
    fi

    if [ -z "$session_id" ]; then
      session_id="$(uuidgen | tr '[:upper:]' '[:lower:]')"
    fi
    if [ -z "$prover_address" ]; then
      prover_address="0x0000000000000000000000000000000000000000"
    fi

    block_payload="$(jq -n -c \
      --argjson start_block "$start_block" \
      --argjson number_of_blocks "$number_of_blocks" \
      --arg l1_head "$l1_head" \
      '{
        start_block_number: $start_block,
        number_of_blocks_to_prove: $number_of_blocks,
        zk_vm: "sp1"
      } + if $l1_head == "" then {} else {l1_head: $l1_head} end')"

    case "$proof_type" in
      compressed)
        request="$(jq -n -c \
          --argjson proof "$block_payload" \
          '{proof_type: "compressed", payload: $proof}')"
        ;;
      snark|plonk|snark_plonk)
        request="$(jq -n -c \
          --argjson proof "$block_payload" \
          --arg prover_address "$prover_address" \
          '{
            proof_type: "snark_plonk",
            payload: {
              proof: $proof,
              prover_address: $prover_address
            }
          }')"
        ;;
      *)
        echo "unknown proof_type: $proof_type (use compressed, snark, or plonk)" >&2
        exit 1
        ;;
    esac

    params="$(jq -n -c \
      --arg session_id "$session_id" \
      --argjson request "$request" \
      '[{
        proof: {
          session_id: $session_id,
          request: $request
        }
      }]')"
    just --justfile {{ source_file() }} _rpc prover_proveBlockRange "$params"

# Get proof status and result
get session_id:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 1; }
    session_id={{ quote(session_id) }}
    params="$(jq -n -c --arg session_id "$session_id" '[{session_id: $session_id}]')"
    just --justfile {{ source_file() }} _rpc prover_getProof "$params"

# Delete a terminal proof request so the same session id can be retried
delete session_id:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 1; }
    session_id={{ quote(session_id) }}
    params="$(jq -n -c --arg session_id "$session_id" '[{session_id: $session_id}]')"
    just --justfile {{ source_file() }} _rpc prover_deleteProofRequest "$params"

# List recent proof requests
list-proofs limit='20' offset='0' status_filter='':
    #!/usr/bin/env bash
    set -euo pipefail
    command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 1; }
    limit={{ quote(limit) }}
    offset={{ quote(offset) }}
    status_filter={{ quote(status_filter) }}
    status_filter="$(printf '%s' "$status_filter" | tr '[:upper:]-' '[:lower:]_')"
    if [ "$status_filter" = "pending" ]; then
      status_filter="queued"
    fi
    case "$limit" in
      ''|*[!0-9]*) echo "limit must be a non-negative integer" >&2; exit 1 ;;
    esac
    case "$offset" in
      ''|*[!0-9]*) echo "offset must be a non-negative integer" >&2; exit 1 ;;
    esac
    case "$status_filter" in
      ""|queued|running|succeeded|failed) ;;
      *)
        echo "unknown status_filter: $status_filter (use queued, running, succeeded, or failed)" >&2
        exit 1
        ;;
    esac

    params="$(jq -n -c \
      --argjson limit "$limit" \
      --argjson offset "$offset" \
      --arg status_filter "$status_filter" \
      '[{
        limit: $limit,
        offset: $offset
      } + if $status_filter == "" then {} else {status_filter: $status_filter} end]')"
    just --justfile {{ source_file() }} _rpc prover_listProofs "$params"

[private]
_rpc method params:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 1; }
    method={{ quote(method) }}
    params={{ quote(params) }}
    payload="$(jq -n -c \
      --arg method "$method" \
      --argjson params "$params" \
      '{
        jsonrpc: "2.0",
        method: $method,
        params: $params,
        id: 1
      }')"
    response_file="$(mktemp)"
    trap 'rm -f "$response_file"' EXIT
    http_status="$(curl -sS \
      --output "$response_file" \
      --write-out '%{http_code}' \
      --header 'content-type: application/json' \
      --data "$payload" \
      "{{ _requester_endpoint }}")"
    if [ -s "$response_file" ]; then
      jq . "$response_file" || cat "$response_file"
    fi
    if [ "$http_status" -ge 400 ]; then
      exit 22
    fi
    if jq -e '.error' "$response_file" >/dev/null 2>&1; then
      exit 1
    fi
