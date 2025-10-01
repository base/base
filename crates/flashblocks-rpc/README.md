### Testing
To include integration tests when testing, run:

```bash
# Must be run in the root of the repo
# Build the base-reth-node binary
cargo build --bin base-reth-node

# Must be run in the crate
# Run the unit tests + integration tests
cargo test --features "integration"
```