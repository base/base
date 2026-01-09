# Contributing

Thanks for your interest in contributing to Base Reth Node.

## Quick start

1. Install Rust (see the version pinned in `Cargo.toml`)
2. Install `just`
3. Clone your fork and run:

```bash
just build
```
```bash
just check   # formatting + clippy checks
just test    # run tests
just fix     # apply formatting + clippy fixes where possible
```

```bash
cargo build --release --bin base-reth-node
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```
