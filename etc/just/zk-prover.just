set positional-arguments := true
set working-directory := '../..'

_script := "etc/scripts/zk-prover/grpc.sh"
_compose := "-f etc/docker/docker-compose.zk-prover.yml"
_yaml_to_env := "etc/scripts/yaml-to-compose-env.py"

# Show ZK prover request commands
default:
    @just --justfile {{ source_file() }} --list --list-prefix '  zk-prover::'

# Start a standalone ZK prover for a network: just zk-prover up network=sepolia mode=dry-run|cluster
up network='devnet' mode='dry-run':
    #!/usr/bin/env bash
    set -euo pipefail
    just --justfile {{ source_file() }} _up "{{ network }}" "{{ mode }}"

# Stop the active standalone ZK prover
down:
    #!/usr/bin/env bash
    set -euo pipefail
    project="$(just --justfile {{ source_file() }} _project)"
    docker compose -p "$project" {{ _compose }} down

# Stream active standalone ZK prover logs
logs *containers:
    #!/usr/bin/env bash
    set -euo pipefail
    project="$(just --justfile {{ source_file() }} _project)"
    docker compose -p "$project" {{ _compose }} logs -f {{ if containers == "" { "base-prover-zk zk-prover-postgres" } else { containers } }}

# List services exposed by the active ZK prover endpoint
list:
    #!/usr/bin/env bash
    set -euo pipefail
    endpoint="$(just --justfile {{ source_file() }} _endpoint)"
    bash "{{ _script }}" list "$endpoint"

# Describe the ZK prover gRPC service
describe:
    #!/usr/bin/env bash
    set -euo pipefail
    endpoint="$(just --justfile {{ source_file() }} _endpoint)"
    bash "{{ _script }}" describe "$endpoint"

# Request a SNARK Groth16 proof for a block range
prove start_block number_of_blocks='1' proof_type='PROOF_TYPE_SNARK_GROTH16' prover_address='':
    #!/usr/bin/env bash
    set -euo pipefail
    endpoint="$(just --justfile {{ source_file() }} _endpoint)"
    bash "{{ _script }}" prove "{{ start_block }}" "{{ number_of_blocks }}" "$endpoint" "{{ proof_type }}" "{{ prover_address }}"

# Get proof status and, for dry-run mode, executionStats
get session_id receipt_type='':
    #!/usr/bin/env bash
    set -euo pipefail
    endpoint="$(just --justfile {{ source_file() }} _endpoint)"
    bash "{{ _script }}" get "{{ session_id }}" "$endpoint" "{{ receipt_type }}"

# Fetch and parse a proof receipt
receipt session_id receipt_type='snark' export_json='':
    #!/usr/bin/env bash
    set -euo pipefail
    endpoint="$(just --justfile {{ source_file() }} _endpoint)"
    bash "{{ _script }}" receipt "{{ session_id }}" "$endpoint" "{{ receipt_type }}" "{{ export_json }}"

# List recent proof requests
list-proofs limit='20' offset='0' status_filter='':
    #!/usr/bin/env bash
    set -euo pipefail
    endpoint="$(just --justfile {{ source_file() }} _endpoint)"
    bash "{{ _script }}" list-proofs "$endpoint" "{{ limit }}" "{{ offset }}" "{{ status_filter }}"

[private]
_project:
    #!/usr/bin/env bash
    set -euo pipefail
    printf 'zk-prover\n'

[private]
_endpoint:
    #!/usr/bin/env bash
    set -euo pipefail
    printf '%s\n' "${ZK_PROVER_ENDPOINT:-localhost:${ZK_PROVER_GRPC_PORT:-9000}}"

[private]
_network-env network output:
    #!/usr/bin/env bash
    set -euo pipefail
    network="{{ network }}"
    network="${network#network=}"
    config="${ZK_NETWORK_CONFIG:-${BASE_NETWORK_CONFIG_DIR:-$HOME/.config/base/networks}/${network}.yaml}"
    if [ ! -f "$config" ]; then
      echo "ERROR: network YAML not found: $config" >&2
      echo "Create it with rpc, l1_rpc, consensus_node_rpc, and l1_beacon_rpc fields." >&2
      exit 1
    fi
    python3 "{{ _yaml_to_env }}" "$config" "{{ output }}" \
      --map rpc=L2_NODE_ADDRESS \
      --map l2_rpc=L2_NODE_ADDRESS \
      --map l1_rpc=L1_NODE_ADDRESS \
      --map consensus_node_rpc=BASE_CONSENSUS_ADDRESS \
      --map op_node_rpc=BASE_CONSENSUS_ADDRESS \
      --map base_consensus_rpc=BASE_CONSENSUS_ADDRESS \
      --map l1_beacon_rpc=L1_BEACON_ADDRESS \
      --map l1_beacon=L1_BEACON_ADDRESS \
      --map beacon_rpc=L1_BEACON_ADDRESS \
      --default ZK_PROVER_NETWORK="$network" \
      --require L2_NODE_ADDRESS \
      --require L1_NODE_ADDRESS \
      --require BASE_CONSENSUS_ADDRESS \
      --require L1_BEACON_ADDRESS

[private]
_cluster-env network output:
    #!/usr/bin/env bash
    set -euo pipefail
    network="{{ network }}"
    network="${network#network=}"
    config="${ZK_CLUSTER_CONFIG:-${BASE_CLUSTER_CONFIG_DIR:-$HOME/.config/base/clusters}/${network}.yaml}"
    if [ ! -f "$config" ]; then
      echo "ERROR: ZK cluster YAML not found: $config" >&2
      echo "Create it with non-secret cluster settings, then retry." >&2
      exit 1
    fi
    python3 "{{ _yaml_to_env }}" "$config" "{{ output }}" \
      --map sp1.prover=SP1_PROVER \
      --map sp1.cluster_api_endpoint=SP1_CLUSTER_API_ENDPOINT \
      --map sp1.cluster_timeout_hours=SP1_CLUSTER_TIMEOUT_HOURS \
      --map artifact.s3_bucket=CLI_S3_BUCKET \
      --map artifact.s3_region=CLI_S3_REGION \
      --map aws.endpoint_url=AWS_ENDPOINT_URL \
      --map aws.default_region=AWS_DEFAULT_REGION \
      --map prover.default_sequence_window=DEFAULT_SEQUENCE_WINDOW \
      --map tls.ca_cert_file=ZK_PROVER_CA_CERT_FILE \
      --map sp1.ca_cert_file=ZK_PROVER_CA_CERT_FILE \
      --map sp1.cluster_ca_cert_file=ZK_PROVER_CA_CERT_FILE \
      --map logging.rust_log=RUST_LOG \
      --default SP1_PROVER=cluster \
      --default SP1_CLUSTER_TIMEOUT_HOURS=24 \
      --default CLI_S3_REGION=us-east-1 \
      --default AWS_DEFAULT_REGION=us-east-1 \
      --require SP1_CLUSTER_API_ENDPOINT \
      --require CLI_S3_BUCKET \
      --deny aws.access_key_id \
      --deny aws.secret_access_key \
      --deny aws.session_token

[private]
_build profile='release':
    #!/usr/bin/env bash
    set -euo pipefail
    case "$(uname -m)" in
      x86_64)       platform_pair="linux-amd64" ;;
      arm64|aarch64) platform_pair="linux-arm64" ;;
      *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
    esac
    ZK_PROVER_PROFILE="{{ profile }}" PLATFORM_PAIR="${platform_pair}" docker buildx bake -f etc/docker/docker-bake.hcl zk-prover --load

[private]
_up network mode:
    #!/usr/bin/env bash
    set -euo pipefail
    mode="{{ mode }}"
    mode="${mode#mode=}"
    network="{{ network }}"
    network="${network#network=}"
    project="$(just --justfile {{ source_file() }} _project)"
    network_env="$(mktemp)"
    cluster_env=""
    ca_compose=""
    trap 'rm -f "$network_env" "$cluster_env" "$ca_compose"' EXIT
    just --justfile {{ source_file() }} _network-env "$network" "$network_env"

    case "$mode" in
      dry-run)
        just --justfile {{ source_file() }} _build "release"
        docker compose -p "$project" --env-file "$network_env" {{ _compose }} down --remove-orphans
        SP1_PROVER=dry-run docker compose -p "$project" --env-file "$network_env" {{ _compose }} up -d --no-build
        ;;
      cluster)
        if [ -z "${AWS_ACCESS_KEY_ID:-}" ] || [ -z "${AWS_SECRET_ACCESS_KEY:-}" ]; then
          echo "ERROR: mode=cluster requires AWS credentials in the caller environment." >&2
          exit 1
        fi
        cluster_env="$(mktemp)"
        just --justfile {{ source_file() }} _cluster-env "$network" "$cluster_env"
        ca_cert_file="${ZK_PROVER_CA_CERT_FILE:-}"
        if [ -z "$ca_cert_file" ]; then
          ca_cert_file="$(awk -F= '$1 == "ZK_PROVER_CA_CERT_FILE" {print substr($0, index($0, "=") + 1)}' "$cluster_env")"
        fi
        compose_files=({{ _compose }})
        if [ -n "$ca_cert_file" ]; then
          if [ ! -f "$ca_cert_file" ]; then
            echo "ERROR: ZK prover CA certificate file not found: $ca_cert_file" >&2
            exit 1
          fi
          ca_compose="$(mktemp)"
          python3 -c 'import json, pathlib, sys; source = json.dumps(sys.argv[1]); pathlib.Path(sys.argv[2]).write_text("services:\n  base-prover-zk:\n    environment:\n      - SSL_CERT_FILE=/etc/ssl/certs/zk-prover-extra-ca.pem\n    volumes:\n      - type: bind\n        source: " + source + "\n        target: /etc/ssl/certs/zk-prover-extra-ca.pem\n        read_only: true\n")' "$ca_cert_file" "$ca_compose"
          compose_files+=(-f "$ca_compose")
        fi
        just --justfile {{ source_file() }} _build "release"
        docker compose -p "$project" --env-file "$network_env" --env-file "$cluster_env" "${compose_files[@]}" down --remove-orphans
        docker compose -p "$project" --env-file "$network_env" --env-file "$cluster_env" "${compose_files[@]}" up -d --no-build
        ;;
      *)
        echo "ERROR: mode must be 'dry-run' or 'cluster' (got '{{ mode }}')" >&2
        exit 1
        ;;
    esac
    printf 'ZK prover active network: %s\n' "$network"
    printf 'ZK prover endpoint: %s\n' "$(just --justfile {{ source_file() }} _endpoint)"
