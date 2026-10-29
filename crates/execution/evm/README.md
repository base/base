# `base-execution-evm`

EVM configuration and execution for Base.

## Overview

Orchestrates Base block execution for Reth on the EVM2 engine. The execution rules live in
`base-common-evm2`; this crate adapts them to Reth's execution traits:

- `BaseEvmConfig` implements Reth's `ConfigureEvm` (and `ConfigureEngineEvm` for engine payloads
  under `std`). It resolves the active Base upgrade from the block timestamp and builds a
  `BaseEvmEnv`: the Base spec, the EVM version (chain ID, gas schedule, nonce and balance check
  toggles, and the Azul EIP-7825 transaction gas cap carried by the Osaka version), and the block
  environment. The block's L1 fee parameters are read from the `L1Block` predeploy during
  execution, at the first non-deposit transaction.
- `BaseBlockExecutorFactory` builds EVMs with Base's transaction registry and the precompile set
  from `BaseEvmTypes::precompiles`, the one place the stateful Beryl and Cobalt precompiles will
  be installed. It creates `BaseRethBlockExecutor`s.
- `BaseRethBlockExecutor` runs the shared `BaseBlockExecutor` (pre-execution system calls,
  Canyon/Denim/Everest transition changes, block gas and Jovian DA-footprint checks, receipts) and
  keeps the block's state as Reth bundle state, streaming every commit, including the
  pre-execution and transition changes, to Reth's state hook. Execution errors map onto Reth's
  block-validation errors; invalid transactions are classified through `BaseInvalidTxError` so
  payload building can skip them. EIP-8130 transactions are rejected as unsupported for now.
- `BaseBlockAssembler` assembles the header: the receipts root (without Regolith deposit nonces
  before Canyon), the Isthmus withdrawals root (the `L2ToL1MessagePasser` storage root), the
  empty requests hash from Isthmus, and from Jovian the block's DA footprint in `blob_gas_used`.
- `BaseEvmEnvBuilder` builds environments for headers, next-block attributes
  (`BaseNextBlockEnvAttributes`), and engine payloads.
- `extract_l1_info` parses the L1-info deposit into `L1FeeParams`, and `RethL1BlockInfo`
  computes L1 data fees from them.
- With the `rpc` feature, `BaseNextBlockEnvAttributes` builds pending-block environments, and
  `BasePendingForecast` forecasts the Denim `BaseTime` state of a pending block.

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
