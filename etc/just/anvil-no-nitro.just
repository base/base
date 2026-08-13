set positional-arguments := true
set working-directory := '../..'

# Show local no-Nitro proof-stack commands
default:
    @just --justfile {{ source_file() }} --list --list-prefix '  anvil-no-nitro::'

# Starts a fresh single-Anvil devnet with no-Nitro contracts, workers, and proposer
up:
    just devnet up-anvil-no-nitro

# Stops proof daemons and removes their fresh Postgres container
down:
    ./etc/scripts/devnet/anvil-no-nitro.sh down

# Prints contract, signer, worker, L2-finality, and latest-game status
status:
    ./etc/scripts/devnet/anvil-no-nitro.sh status

# Tails prover-service, Nitro worker, and proposer logs
logs:
    ./etc/scripts/devnet/anvil-no-nitro.sh logs
