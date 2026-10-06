set positional-arguments
set working-directory := '../..'

alias start := up
alias stop := down

# Show snapshot devnet commands
default:
    @just --justfile {{ source_file() }} --list --list-prefix '  just devnet snapshot '

# Initialize or resume a fork from existing writable snapshot copies
init *args:
    python3 etc/scripts/devnet/snapshot_devnet.py init "$@"

# Start or resume a prepared fork without deleting data
up *args:
    python3 etc/scripts/devnet/snapshot_devnet.py up "$@"

# Stop a fork and preserve all data
down *args:
    python3 etc/scripts/devnet/snapshot_devnet.py down "$@"

# Show fork phase, containers and private RPC addresses
status *args:
    python3 etc/scripts/devnet/snapshot_devnet.py status "$@"

# Retire a stopped fork directory without deleting its datadirs
reset *args:
    python3 etc/scripts/devnet/snapshot_devnet.py reset "$@"

# Offline launcher tests
test:
    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s etc/scripts/devnet -p test_snapshot_devnet.py -v
