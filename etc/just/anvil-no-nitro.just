set positional-arguments := true
set working-directory := '../..'

# Show anvil no-Nitro prover stack commands
default:
    @just --justfile {{ source_file() }} --list --list-prefix '  anvil-no-nitro::'

# Starts the local no-Nitro stack, using Anvil for smoke tests or L1_RPC_URL for real proofs against a live L2
up:
    ./etc/scripts/anvil-no-nitro.sh up

# Stops all daemons and removes the Postgres container
down:
    ./etc/scripts/anvil-no-nitro.sh down

# Re-syncs the ProtocolVersions ladder from the live L2's rollup config
sync:
    ./etc/scripts/anvil-no-nitro.sh sync

# Prints daemon, contract, signer, and latest game status
status:
    ./etc/scripts/anvil-no-nitro.sh status

# Tails the anvil, prover-service, nitro prover, and proposer logs
logs:
    ./etc/scripts/anvil-no-nitro.sh logs
