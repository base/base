# `base-execution-evm-blocks`

Base block execution, assembly, and validation.

This crate configures the Base EVM, executes and assembles blocks, and validates headers, bodies, receipts, and upgrade-specific commitments. It combines the former execution EVM and execution consensus packages while keeping the pure EVM runtime below the node-facing executor.

Public APIs are re-exported from the crate root. The `test-utils` feature exposes execution fixtures and enables the `block_execution` integration suite:

```sh
cargo test -p base-execution-evm-blocks --all-targets --features test-utils
```
