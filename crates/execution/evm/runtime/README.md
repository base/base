# `base-execution-evm-runtime`

Base transaction execution and the shared EVM handler in one crate.

The runtime owns Base transaction rules, execution handlers, frame processing, execution APIs, state hooks, and inspector integration. Context/environment types, memory state, interpreter operations, and native precompile dispatch remain lower-level dependencies.

`BaseEvm`, `BaseEvmFactory`, `BaseHandler`, and `BaseTransaction` provide the Base execution path. Shared execution and inspection APIs remain available for proof execution and test harnesses. The block-level node executor lives in `base-execution-evm-blocks`.

The `std`, tracing, crypto backend, and execution-check features describe actual build capabilities. RPC request conversion does not require a feature switch; there is no separate `reth` feature.

```sh
cargo test -p base-execution-evm-runtime --lib --tests
cargo check -p base-execution-evm-runtime --no-default-features --target riscv32imac-unknown-none-elf
```

This crate combines the former Base EVM and local REVM handler packages. Historical execution upgrade behavior and existing tracing APIs are retained.
