set positional-arguments := true
set working-directory := '../..'

_compose_file := "etc/docker/docker-compose.prover.yml"

# Show standalone prover commands
default:
    @just --justfile {{ source_file() }} --list --list-prefix '  prover::'

# Builds and starts a network-scoped standalone prover stack
up network:
    #!/usr/bin/env bash
    set -euo pipefail

    network={{ quote(network) }}
    network="${network#network=}"
    if ! [[ "$network" =~ ^[a-z0-9][a-z0-9_-]*$ ]]; then
      echo "ERROR: network must match ^[a-z0-9][a-z0-9_-]*$ (got '${network}')" >&2
      exit 1
    fi

    missing=()
    for var in L1_NODE_ADDRESS L1_BEACON_ADDRESS L2_NODE_ADDRESS BASE_CONSENSUS_ADDRESS; do
      if [ -z "${!var:-}" ]; then
        missing+=("$var")
      fi
    done
    if [ "${#missing[@]}" -gt 0 ]; then
      echo "ERROR: missing required environment variables: ${missing[*]}" >&2
      echo "Export your RPC endpoints, then retry." >&2
      echo "See docs/guides/STANDALONE_PROVING.md for the full workflow." >&2
      exit 1
    fi

    if [ -z "${NETWORK_PRIVATE_KEY:-}" ]; then
      echo "NOTE: NETWORK_PRIVATE_KEY is not set; starting a dry-run-only stack." >&2
      echo "The paid network backend stays disabled until the stack is restarted with a funded requester key." >&2
    fi

    for elf in range-elf-embedded aggregation-elf; do
      elf_path="crates/proof/zk/programs/succinct/elf/${elf}"
      if [ ! -s "$elf_path" ]; then
        echo "ERROR: real SP1 ELF not found: ${elf_path}" >&2
        echo "Paid network proving requires reproducible ELFs. Build them first:" >&2
        echo "  just succinct build-elfs" >&2
        exit 1
      fi
    done

    case "$(uname -m)" in
      x86_64)        platform_pair="linux-amd64" ;;
      arm64|aarch64) platform_pair="linux-arm64" ;;
      *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
    esac
    PROFILE=dev ZK_HOST_PROFILE=release BASE_SUCCINCT_ELF_REQUIRE=1 PLATFORM_PAIR="${platform_pair}" \
      docker buildx bake -f etc/docker/docker-bake.hcl prover-service zk-host --load

    PROVER_NETWORK="$network" docker compose -p "base-prover-${network}" \
      -f "{{ _compose_file }}" up -d --no-build --remove-orphans

# Stops one standalone prover stack while preserving its network-scoped Postgres data
down network:
    #!/usr/bin/env bash
    set -euo pipefail
    network={{ quote(network) }}
    network="${network#network=}"
    if ! [[ "$network" =~ ^[a-z0-9][a-z0-9_-]*$ ]]; then
      echo "ERROR: invalid network label: ${network}" >&2
      exit 1
    fi
    PROVER_NETWORK="$network" docker compose -p "base-prover-${network}" \
      -f "{{ _compose_file }}" down

# Streams logs from one standalone prover stack
logs network *containers:
    #!/usr/bin/env bash
    set -euo pipefail
    network={{ quote(network) }}
    network="${network#network=}"
    if ! [[ "$network" =~ ^[a-z0-9][a-z0-9_-]*$ ]]; then
      echo "ERROR: invalid network label: ${network}" >&2
      exit 1
    fi
    PROVER_NETWORK="$network" docker compose -p "base-prover-${network}" \
      -f "{{ _compose_file }}" logs -f {{ containers }}
