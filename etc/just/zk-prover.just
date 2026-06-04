set positional-arguments := true
set working-directory := '../..'

_script := "etc/scripts/zk-prover/grpc.sh"
_ca_compose := "etc/scripts/zk-prover/ca-compose.sh"
_ca_combined_file := "/tmp/zk-prover-ca-certificates.crt"
_ca_extra_file := "/etc/ssl/certs/zk-prover-extra-ca.pem"
_ca_service := "base-prover-zk"
_ca_service_command := "./base-prover-zk"
_ca_system_file := "/etc/ssl/certs/ca-certificates.crt"
_compose := "-f etc/docker/docker-compose.zk-prover.yml"
_endpoint := "${ZK_PROVER_ENDPOINT:-localhost:${ZK_PROVER_GRPC_PORT:-9000}}"
_project := "zk-prover"
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
    docker compose -p "{{ _project }}" {{ _compose }} down --remove-orphans

# Stream active standalone ZK prover logs
logs *containers:
    #!/usr/bin/env bash
    set -euo pipefail
    docker compose -p "{{ _project }}" {{ _compose }} logs -f {{ if containers == "" { "base-prover-zk zk-prover-postgres" } else { containers } }}

# List services exposed by the active ZK prover endpoint
list:
    bash "{{ _script }}" list "{{ _endpoint }}"

# Describe the ZK prover gRPC service
describe:
    bash "{{ _script }}" describe "{{ _endpoint }}"

# Request a proof or dry-run for a block range: proof_type=compressed|snark
prove start_block number_of_blocks='1' proof_type='compressed':
    #!/usr/bin/env bash
    set -euo pipefail
    proof_type="{{ proof_type }}"
    proof_type="${proof_type#proof_type=}"
    bash "{{ _script }}" prove "{{ start_block }}" "{{ number_of_blocks }}" "{{ _endpoint }}" "$proof_type"

# Get proof status and, for dry-run mode, executionStats
get session_id receipt_type='':
    bash "{{ _script }}" get "{{ session_id }}" "{{ _endpoint }}" "{{ receipt_type }}"

# Fetch and parse a proof receipt
receipt session_id receipt_type='snark' export_json='':
    bash "{{ _script }}" receipt "{{ session_id }}" "{{ _endpoint }}" "{{ receipt_type }}" "{{ export_json }}"

# List recent proof requests
list-proofs limit='20' offset='0' status_filter='':
    bash "{{ _script }}" list-proofs "{{ _endpoint }}" "{{ limit }}" "{{ offset }}" "{{ status_filter }}"

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
    network_env="$(mktemp)"
    cluster_env=""
    ca_compose=""
    cleanup_temp_files() {
      rm -f "$network_env"
      if [ -n "$cluster_env" ]; then
        rm -f "$cluster_env"
      fi
      if [ -n "$ca_compose" ]; then
        rm -f "$ca_compose"
      fi
    }
    trap cleanup_temp_files EXIT
    just --justfile {{ source_file() }} _network-env "$network" "$network_env"

    case "$mode" in
      dry-run)
        just --justfile {{ source_file() }} _build "release"
        docker compose -p "{{ _project }}" --env-file "$network_env" {{ _compose }} down --remove-orphans
        SP1_PROVER=dry-run docker compose -p "{{ _project }}" --env-file "$network_env" {{ _compose }} up -d --no-build
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
          bash "{{ _ca_compose }}" "$ca_cert_file" "$ca_compose" \
            --service-name "{{ _ca_service }}" \
            --service-command "{{ _ca_service_command }}" \
            --system-ca-file "{{ _ca_system_file }}" \
            --extra-ca-file "{{ _ca_extra_file }}" \
            --combined-ca-file "{{ _ca_combined_file }}"
          compose_files+=(-f "$ca_compose")
        fi
        just --justfile {{ source_file() }} _build "release"
        docker compose -p "{{ _project }}" --env-file "$network_env" --env-file "$cluster_env" "${compose_files[@]}" down --remove-orphans
        docker compose -p "{{ _project }}" --env-file "$network_env" --env-file "$cluster_env" "${compose_files[@]}" up -d --no-build
        ;;
      *)
        echo "ERROR: mode must be 'dry-run' or 'cluster' (got '{{ mode }}')" >&2
        exit 1
        ;;
    esac
    printf 'ZK prover active network: %s\n' "$network"
    printf 'ZK prover endpoint: %s\n' "{{ _endpoint }}"
