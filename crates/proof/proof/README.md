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
use base_proof::{OraclePipeline, BaseExecutor, CachingOracle};

let oracle = Arc::new(CachingOracle::new(raw_oracle));
let pipeline = OraclePipeline::new(cfg, oracle.clone(), l1_provider, l2_provider);
let executor = BaseExecutor::new(chain_spec);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
