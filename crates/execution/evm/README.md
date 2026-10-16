# `base-execution-evm`

EVM configuration and execution for Base.

## Overview

Orchestrates EVM block execution for Base chains. The `BaseEvmConfig` type implements Reth's
`ConfigureEvm` and `ConfigureEngineEvm` traits, constructing upgrade-aware execution environments
by mapping timestamps to `SpecId` values and building the correct EVM context for each block.
Also provides `BaseEvmEnvBuilder`, `BaseBlockAssembler`, L1 block info parsing via `extract_l1_info`,
L1 data-fee calculation via `RethL1BlockInfo`, `BaseRethReceiptBuilder`, and crate-specific error types.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-evm = { workspace = true }
```

```rust,ignore
use base_execution_evm::BaseEvmConfig;
use reth_evm::ConfigureEvm;

let evm_config = BaseEvmConfig::base(chain_spec);
let env = evm_config.evm_env(&header)?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
