# base-genesis

Generate an offline devnet using `base/contracts`, Forge, and Lighthouse.
Enabled through the optional `genesis` feature on `base`.

```sh
# Build the contract bundle (Git, Bash, jq, and Forge 1.8.1 required).
just build genesis-contracts

# Generate genesis files (Bash, jq, flock, and Forge required).
just genesis --output-dir .devnet/genesis

# Rebuild contracts from a local checkout instead.
just build genesis-contracts /path/to/contracts
```

`just genesis` builds the Rust command and runs `etc/genesis/generate.sh`.
The script invokes Forge; Rust prepares inputs, computes the L2 anchor, and writes
execution genesis, beacon state, validator keys, and rollup configs under
`el/`, `cl/`, and `l2/`. Use `base genesis --help` for configuration options.

Identical runs validate and reuse existing outputs. Use a fresh output directory
after changing configuration or rebuilding contracts. Generated accounts and
validator keys are publicly known and intended for development only.

```sh
cargo test -p base-genesis

# Integration tests require the contract bundle and Forge.
cargo build -p base-genesis --example generate
cargo test -p base-genesis --lib -- --include-ignored
```
