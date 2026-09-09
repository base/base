# `base-execution-evm-crypto`

Locally maintained EVM crypto precompiles and their backend selection, formerly `revm-precompile`.

This crate implements hashing, signature recovery and verification, modular exponentiation,
elliptic-curve operations, and KZG point evaluation. It owns crypto precompile IDs, gas accounting,
results, and the fork-specific Ethereum precompile sets. Base native contracts and their storage
logic live in `base-execution-evm-precompiles`.

Existing crypto backend features and historical fork behavior are retained. Standard-library and
native acceleration features can be disabled for proof targets.

```sh
cargo test -p base-execution-evm-crypto
cargo check -p base-execution-evm-crypto --no-default-features --target riscv32imac-unknown-none-elf
```
