# `base-execution-evm`

EVM configuration and execution for Base.

## Overview

Orchestrates EVM block execution for Base chains. The `BaseEvmConfig` type implements Reth's
`ConfigureEvm` and `ConfigureEngineEvm` traits, constructing hardfork-aware execution environments
by mapping timestamps to `SpecId` values and building the correct EVM context for each block.
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

let evm_config = BaseEvmConfig::optimism(chain_spec);
let env = evm_config.evm_env(&header, &parent)?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
