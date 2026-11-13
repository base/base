# `base-proof`

`base-proof` is a Base state transition proof SDK.

## Overview

Provides the L1/L2 data sources and block execution infrastructure needed for fault proof
generation via the preimage oracle. Includes `OraclePipeline` for oracle-backed derivation,
`OracleL1ChainProvider` and `OracleL2ChainProvider` for fetching chain data, `CachingOracle` for
efficient preimage access, and `BaseExecutor` implementing the `Executor` trait for the proof
driver. Also handles the boot phase (reading boot info from the oracle) and EIP-2935 history
lookups.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-proof = { workspace = true }
```

```rust,ignore
use base_proof::{BaseExecutor, CachingOracle, OraclePipeline};

// `oracle_reader` and `hint_writer` speak the preimage protocol over the host channels.
let oracle = Arc::new(CachingOracle::new(cache_size, oracle_reader, hint_writer));

// `cursor` comes from `new_oracle_pipeline_cursor`; the providers are the oracle-backed
// `OracleL1ChainProvider` / `OracleL2ChainProvider` and a data availability source.
let pipeline = OraclePipeline::new(
    rollup_config.clone(),
    l1_config,
    cursor.clone(),
    oracle.clone(),
    da_provider,
    l1_provider,
    l2_provider.clone(),
)
.await?;

// The L2 provider serves both trie nodes and trie hints.
let executor =
    BaseExecutor::new(&rollup_config, l2_provider.clone(), l2_provider, evm_factory, None);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
