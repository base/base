# Show snapshot dev chain commands
default:
    @just --justfile {{ source_file() }} --list

# Downloads full state with 1,339,200 blocks of history; never starts a node
[no-cd]
[positional-arguments]
download *args:
    exec cargo run --release --manifest-path {{ quote(source_directory() / "../../Cargo.toml") }} --bin base -- snapshot download --with-txs-distance 1339200 --with-receipts-distance 1339200 --with-state-history-distance 1339200 "$@"

# Builds base and runs `base dev` in the foreground, e.g. `run --dir ~/data/devnet`
[no-cd]
[positional-arguments]
run *args:
    exec cargo run --release --manifest-path {{ quote(source_directory() / "../../Cargo.toml") }} --bin base -- dev "$@"
