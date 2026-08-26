set positional-arguments := true
set working-directory := '../..'

# Show local Nitro proof-stack commands
default:
    @just --justfile {{ source_file() }} --list --list-prefix '  anvil-nitro-local::'

# Starts a fresh single-Anvil devnet with local Nitro workers and proposer
up:
    just devnet up-anvil-nitro-local

# Stops proof daemons and removes their fresh Postgres container
down:
    ./etc/scripts/devnet/anvil-nitro-local.sh down

# Prints contract, signer, worker, L2-finality, and latest-game status
status:
    ./etc/scripts/devnet/anvil-nitro-local.sh status

# Tails prover-service, Nitro worker, and proposer logs
logs:
    ./etc/scripts/devnet/anvil-nitro-local.sh logs
