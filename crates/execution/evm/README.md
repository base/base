# `base-execution-evm`

EVM configuration and execution for Base.

## Overview

Provides Base's concrete EVM configuration through the shared `reth-evm` crate.
`BaseEvmConfig` constructs execution environments from the chain's upgrade schedule
and builds the EVM context for each block.
Re-exports executor factories, block executors, and error types from the underlying alloy/revm
layers.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-evm = { workspace = true }
```

```rust,ignore
use base_execution_evm::BaseEvmConfig;

let evm_config = BaseEvmConfig::new(chain_spec);
let env = evm_config.evm_env(&header)?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
