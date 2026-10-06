set positional-arguments
set working-directory := '../..'

# Show snapshot devnet commands
default:
    @just --justfile {{ source_file() }} --list --list-prefix '  just devnet snapshot '

# Initialize or resume a fork from existing writable snapshot copies
init *args:
    python3 etc/scripts/devnet/snapshot_devnet.py init "$@"

# Offline launcher tests
test:
    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s etc/scripts/devnet -p test_snapshot_devnet.py -v
