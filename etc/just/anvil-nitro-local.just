set positional-arguments := true
set working-directory := '../..'

_anvil := "-f etc/docker/docker-compose.yml -f etc/docker/docker-compose.anvil-l1.yml"
_env := "--env-file etc/docker/devnet-env"
_services := "base-proofs base-proofs-consensus prover-service-postgres base-prover-service nitro-prover-1 nitro-prover-2 base-proposer"

# Show local Nitro proof-stack commands
default:
    @just --justfile {{ source_file() }} --list --list-prefix '  anvil-nitro-local::'

# Starts a fresh single-Anvil devnet with local Nitro workers and proposer
up:
    just devnet up-anvil-nitro-local

# Stops the local Nitro devnet and removes its data
down:
    just devnet down-anvil

# Prints container, contract, signer, L2-finality, and latest-game status
status:
    docker compose {{ _env }} {{ _anvil }} --profile nitro-local ps {{ _services }}
    ./etc/scripts/devnet/anvil-nitro-local.sh status

# Tails proof node, prover-service, Nitro worker, and proposer logs
logs:
    docker compose {{ _env }} {{ _anvil }} --profile nitro-local logs --tail 50 -f {{ _services }}
