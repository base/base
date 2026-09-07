# revmc-codegen

EVM bytecode compiler frontend and code generation pipeline.

This crate contains the bytecode parser and analysis passes, the generic compiler driver, linker helpers, and test utilities for producing JIT and AOT artifacts through compiler backends such as `revmc-llvm`.

For the runtime worker pool and hot-code lookup backend, see `revmc-runtime`. For the umbrella crate, see `revmc`.
