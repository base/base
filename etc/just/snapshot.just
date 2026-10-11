# Show snapshot dev chain commands
default:
    @just --justfile {{ source_file() }} --list

# Builds base and runs `base dev` in the foreground, e.g. `run --dir ~/data/devnet`
[no-cd]
[positional-arguments]
run *args:
    exec cargo run --release --manifest-path {{ quote(source_directory() / "../../Cargo.toml") }} --bin base -- dev "$@"
