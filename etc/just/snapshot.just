# Show snapshot dev chain commands
default:
    @just --justfile {{ source_file() }} --list

# Downloads the full preset limited to 1,339,200 blocks of history; never starts a node
[no-cd]
[positional-arguments]
download *args:
    #!/usr/bin/env bash
    set -euo pipefail
    dir= prev=
    for arg in "$@"; do
        case "$prev" in --dir|--datadir) dir=$arg ;; esac
        case "$arg" in --dir=*|--datadir=*) dir=${arg#*=} ;; esac
        prev=$arg
    done
    if [ -z "$dir" ]; then
        echo "error: pass --dir <datadir>" >&2
        exit 1
    fi
    existing_config=false
    if [ -e "$dir/reth.toml" ]; then existing_config=true; fi
    # `--full` keeps 13,392,000 blocks: 31 days at Denim's 200ms, but about 310 days of pre-Denim
    # 2s blocks. Keep 1,339,200 blocks (31 days at 2s) until Denim has run for 31 days.
    cargo run --release --manifest-path {{ quote(source_directory() / "../../Cargo.toml") }} --bin base -- snapshot download --download-concurrency 16 --with-txs-distance 1339200 --with-receipts-distance 1339200 --with-state-history-distance 1339200 "$@"
    # Explicit selections prune the transaction-hash index; `--full` keeps it.
    if [ "$existing_config" = false ] && [ -f "$dir/reth.toml" ]; then
        sed -i '/^transaction_lookup = "full"$/d' "$dir/reth.toml"
    fi

# Builds base and runs `base dev` in the foreground, e.g. `run --dir ~/data/devnet`
[no-cd]
[positional-arguments]
run *args:
    exec cargo run --release --manifest-path {{ quote(source_directory() / "../../Cargo.toml") }} --bin base -- dev "$@"
