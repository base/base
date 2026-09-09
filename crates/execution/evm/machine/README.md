# `base-execution-evm-machine`

Execution contexts, transaction and block environments, journaling, and the EVM bytecode interpreter.

The context and interpreter share gas accounting, host interfaces, state access, and execution
results in one crate. The instruction set, stack, memory, and call/create actions live alongside
the environments they consume. This combines Base's execution context and the locally maintained
REVM interpreter; node startup and persistent database implementations remain in their owners.

The crate supports allocation-only execution with default features disabled. `serde` enables
serialization, and the existing optional validation and memory-limit features are retained for
execution and simulation callers.

```sh
cargo test -p base-execution-evm-machine
cargo check -p base-execution-evm-machine --no-default-features --target riscv32imac-unknown-none-elf
```
