# `base-execution-evm-runtime`

Base execution, interpreter operations, memory state, cryptographic precompiles, and native
precompile dispatch in one crate. Procedural macros remain in `base-execution-evm-macros`.

`Context<DB>` fixes the transaction, configuration, and chain state to Base. `BaseEvm<DB, I>`
retains database and inspector parameters and uses `PrecompilesMap`, including its dynamic
callbacks and caching support. `BaseBlockExecutorFactory` uses `ChainConfig` and `BaseEvmFactory`.
The node's block execution configuration lives in `base-execution-evm-blocks`.

Ethereum reference constructors and `ReferenceContext` are exposed only by `test-utils`.
Concrete tracing implementations, including the optional JavaScript tracer, live in
`base-execution-rpc`. The runtime retains the inspector interface.

`std`, tracing, and the retained cryptographic features describe build capabilities. Proof
execution continues to support bare-metal builds without `std`.
