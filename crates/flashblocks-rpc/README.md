### Testing
To include integration tests when testing, run:

```bash
# Build the op-rbuilder binary
cargo build --bin base-reth-node

# Run the unit tests + integration tests
cargo test --features "integration"
```