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

`BASE_DEVNET_VALIDATOR_COUNT` (or `--validator-count`) selects the number of
validators in beacon genesis and the matching validator keystores. It defaults
to `1` and must be a positive decimal integer within the 32-bit validator
derivation index range. For example, `BASE_DEVNET_VALIDATOR_COUNT=64 just devnet up`
starts a fresh devnet with 64 validators. Set it before generating fresh state;
identical completed outputs are reused, while a changed count requires a fresh
output directory.

Identical runs validate and reuse existing outputs. Use a fresh output directory
after changing configuration or rebuilding contracts. Generated accounts and
validator keys are publicly known and intended for development only.

```sh
cargo test -p base-genesis

# Integration tests require the contract bundle and Forge.
cargo build -p base --features genesis
cargo test -p base-genesis --lib -- --include-ignored
```
