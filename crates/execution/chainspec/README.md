# `base-execution-chainspec`

## Overview

Provides `BaseChainSpec`, the chain specification type for Base nodes. Includes upgrade-specific
base fee computation helpers for Holocene and Jovian, and supported chain resolution from CLI
strings.

## How it works

`ChainConfig` owns the network identity, typed Base upgrade schedule, consensus genesis metadata,
fee parameters, and activation admin. `BaseChainSpec` combines that configuration with the parsed
genesis and its derived execution header. Ethereum execution rules are derived from Base upgrades;
execution, consensus, and fork IDs resolve the same runtime activation overrides.

The genesis header is derived at startup from the genesis JSON using
`BaseChainSpec::make_genesis_header`, which computes the correct state root, storage root, and
other fields for Base.

Chain names are resolved from CLI strings via `SUPPORTED_CHAINS`, which maps `"base"`,
`"base_sepolia"`, `"base-sepolia"`, and `"dev"` to specs built from `base-common-chains`.

### Base fee computation

Two helpers handle upgrade-specific base fee logic:

- `decode_holocene_base_fee` - Reads the EIP-1559 elasticity and denominator packed into the
  parent block's `extra_data` field (per the Holocene spec). If both are zero, falls back to the
  chain spec's default params for that timestamp.
- `compute_jovian_base_fee` - Extends Holocene logic with a minimum base fee floor also encoded in
  `extra_data`, and uses `max(gas_used, blob_gas_used)` as the effective gas used for the
  next-block fee calculation.

Gas limits and other chain parameters are sourced from
[`base_common_chains::ChainConfig`](../../../crates/common/chains/src/config.rs).

## Usage

Add the crate to your `Cargo.toml`:

```toml,ignore
base-execution-chainspec = { workspace = true }
```

Build a chain spec from common chain config:

```rust,ignore
use base_execution_chainspec::BaseChainSpec;

let spec = BaseChainSpec::mainnet();
println!("chain: {}", spec.chain());
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
