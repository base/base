### Testing
To include integration tests when testing, run:

```bash
# Build the op-rbuilder binary
cargo build --bin reth-flashblocks

# Run the unit tests + integration tests
cargo test --features "integration"
```