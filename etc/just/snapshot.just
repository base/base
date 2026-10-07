set positional-arguments
set working-directory := '../..'

alias start := up
alias stop := down

# Show snapshot devnet commands
default:
    @just --justfile {{ source_file() }} --list --list-prefix '  just devnet snapshot '

# Prepare and initialize an experiment; reuse completed steps and leave services stopped
setup *args:
    python3 etc/scripts/devnet/snapshot_devnet.py setup "$@"

# Download one datadir without copying it or configuring a devnet
download *args:
    python3 etc/scripts/devnet/snapshot_devnet.py download "$@"

# Initialize an existing datadir in place, including Denim; optionally add --validator-dir
init *args:
    python3 etc/scripts/devnet/snapshot_devnet.py init "$@"

# Start the initialized fork; catch-up continues in the background
up *args:
    python3 etc/scripts/devnet/snapshot_devnet.py up "$@"

# Stop the configured fork and preserve all data
down *args:
    python3 etc/scripts/devnet/snapshot_devnet.py down "$@"

# Stop saved experiments and forget their selection; keep databases and fork state
clear *args:
    python3 etc/scripts/devnet/snapshot_devnet.py clear "$@"

# Show fork services, block heights, and Denim scheduling
status *args:
    python3 etc/scripts/devnet/snapshot_devnet.py status "$@"

# Retire a stopped fork directory without deleting its datadirs
reset *args:
    python3 etc/scripts/devnet/snapshot_devnet.py reset "$@"

# Schedule Denim at the earliest permitted time or a supplied timestamp
schedule-denim *args:
    python3 etc/scripts/devnet/snapshot_devnet.py schedule-denim "$@"

# Fund the local test user through the L1 portal
deposit *args:
    python3 etc/scripts/devnet/snapshot_devnet.py deposit "$@"

# Verify derivation, Denim, restarts and interrupted recovery (disruptive; start before Denim)
verify *args:
    python3 etc/scripts/devnet/snapshot_verify.py "$@"

# Offline launcher tests (BASE_SNAPSHOT_FORK_DIR opts into live qualification)
test:
    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s etc/scripts/devnet -p 'test_snapshot_*.py' -v

# Build the pinned fork-aware Beacon and durable-resume Anvil revision
build-anvil:
    docker buildx build --load --tag base-anvil:snapshot-24ec5e47 \
      --build-arg RUST_PROFILE=release --build-arg RUST_FEATURES=anvil/cli \
      'https://github.com/base/base-anvil.git#24ec5e4732f35b904c43cb05a33fd459e6e92cb8'
