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

    docker_engine_version="$(docker version --format '{{ "{{" }}.Server.Version{{ "}}" }}' 2>/dev/null)" || {
      echo "ERROR: standalone proving requires Docker Engine 28.3.3 or later." >&2
      echo "Older releases can expose loopback-published ports to the local network." >&2
      exit 1
    }
    if ! [[ "$docker_engine_version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)(\+[[:alnum:].-]+)?$ ]] ||
      (( 10#${BASH_REMATCH[1]} < 28 ||
         (10#${BASH_REMATCH[1]} == 28 &&
          (10#${BASH_REMATCH[2]} < 3 ||
           (10#${BASH_REMATCH[2]} == 3 && 10#${BASH_REMATCH[3]} < 3))) )); then
      echo "ERROR: Docker Engine 28.3.3 or later is required (found '${docker_engine_version}')." >&2
      echo "Older releases can expose loopback-published ports to the local network." >&2
      exit 1
    fi

    prover_network="base-prover-${network}_default"
    if docker network inspect "$prover_network" >/dev/null 2>&1; then
      gateway_mode_v4="$(docker network inspect --format '{{ "{{" }} index .Options "com.docker.network.bridge.gateway_mode_ipv4" {{ "}}" }}' "$prover_network")"
      gateway_mode_v6="$(docker network inspect --format '{{ "{{" }} index .Options "com.docker.network.bridge.gateway_mode_ipv6" {{ "}}" }}' "$prover_network")"
      trusted_host_interfaces="$(docker network inspect --format '{{ "{{" }} index .Options "com.docker.network.bridge.trusted_host_interfaces" {{ "}}" }}' "$prover_network")"
      if [[ "$gateway_mode_v4" != "nat" || "$gateway_mode_v6" != "nat" || -n "$trusted_host_interfaces" ]]; then
        echo "ERROR: existing Docker network '${prover_network}' does not safely restrict direct routing." >&2
        echo "Run 'just prover down ${network}' to remove it, then retry." >&2
        exit 1
      fi
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

    # The compose file publishes the requester RPC as 127.0.0.1:<port>:9000, so
    # this override fills the host-port position only.
    if [ -n "${PROVER_SERVICE_RPC_PORT:-}" ] && ! [[ "$PROVER_SERVICE_RPC_PORT" =~ ^[0-9]+$ ]]; then
      echo "ERROR: PROVER_SERVICE_RPC_PORT must be a bare port number (got '${PROVER_SERVICE_RPC_PORT}')" >&2
      echo "The requester RPC is unauthenticated and is published to loopback only." >&2
      echo "To reach it from another host, front it with your own authenticated proxy." >&2
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

    PROFILE=dev ZK_HOST_PROFILE=release BASE_SUCCINCT_ELF_REQUIRE=1 \
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
