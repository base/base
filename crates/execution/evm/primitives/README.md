# `base-execution-evm-primitives`

EVM constants, execution fork identifiers, storage aliases, bytecode analysis, and opcode metadata.
This combines the locally maintained REVM primitive and bytecode crates.

`Bytecode` owns analyzed legacy code or EIP-7702 delegation code. Jump tables, opcode iteration,
decode errors, and delegation constants live beside the execution rules they use. The execution
fork identifiers retain historical Ethereum semantics; the Base upgrade schedule remains in
`base-common-chain-config`.

Default features enable standard-library support and opcode text parsing. Disable defaults for
allocation-only proof execution; `serde` and `arbitrary` remain separate features.

```sh
cargo test -p base-execution-evm-primitives
cargo check -p base-execution-evm-primitives --no-default-features --target riscv32imac-unknown-none-elf
```
