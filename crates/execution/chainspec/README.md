# `base-execution-chainspec`

## Overview

Provides `BaseChainSpec`, the chain specification type for Base nodes. Includes upgrade-specific
base fee computation helpers for Holocene and Jovian, and supported chain resolution from CLI
strings.

## How it works

`BaseChainSpec` wraps reth's `ChainSpec` and adds Base-specific upgrade awareness via the
`BaseUpgrades` trait. Network specs are converted from `base-common-chains` configs, which own the
genesis JSON, upgrade schedule, base fee params, and other chain constants.

The genesis header is derived at startup from the genesis JSON using
`BaseChainSpec::make_genesis_header`, which computes the correct state root, storage root, and
other fields for Base.

Chain names are resolved from CLI strings via `SUPPORTED_CHAINS`, which maps `"base"`,
`"base_sepolia"`, `"base-sepolia"`, and `"dev"` to specs built from `base-common-chains`.

### Denim timestamps

Execution validates Denim timestamps against the same block-number schedule as consensus.
The schedule uses the genesis header's number and timestamp, the legacy block interval, and
the runtime-aware Denim activation time. Built-in networks supply the interval from `ChainConfig`;
integrated `base rpc`, `base sequencer`, and `base follow` use the loaded rollup config's
`block_time`. Standalone execution nodes using a custom genesis must set a nonzero
`config.blockTime` (seconds). Once Denim is scheduled, validation rejects missing or zero
intervals rather than assuming a two-second cadence.

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
