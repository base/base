# `base-execution-evm-fees`

Base transaction compression estimation, L1 data fee calculation, and operator fees.

`L1FeeParams` holds the fee parameters supplied by L1 attributes. It shares FastLZ length
estimation and Fjord size calculations with the transaction pool, bundle metering, and payload
builder. Historical execution fee rules remain available.

The implementation supports `no_std`. Compression benchmarks remain alongside the fee code.

```sh
cargo test -p base-execution-evm-fees
cargo check -p base-execution-evm-fees --no-default-features --target riscv32imac-unknown-none-elf
```
