## Testing

To include integration tests when testing, run:

```bash
# Generate a genesis file
cargo run --bin utils -- genesis --output genesis.json

# Build the op-rbuilder binary
cargo build --bin reth-flashblocks

# Run the unit tests + integration tests
cargo test --features "optimism,integration"
```