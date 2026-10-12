# Show snapshot dev chain commands
default:
    @just --justfile {{ source_file() }} --list

# Downloads the minimal snapshot preset; never starts a node
[no-cd]
[positional-arguments]
download *args:
    exec cargo run --release --manifest-path {{ quote(source_directory() / "../../Cargo.toml") }} --bin base -- snapshot download --minimal "$@"

# Builds base and runs `base dev` in the foreground, e.g. `run --dir ~/data/devnet`
[no-cd]
[positional-arguments]
run *args:
    exec cargo run --release --manifest-path {{ quote(source_directory() / "../../Cargo.toml") }} --bin base -- dev "$@"
