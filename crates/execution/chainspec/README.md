# `base-execution-chainspec`

## Overview

Provides `OpChainSpec`, the chain specification type for Base nodes, along with pre-built specs
for Base Mainnet (8453), Base Sepolia (84532), and a local dev chain. Includes hardfork-specific
base fee computation helpers for Holocene and Jovian, and supported chain resolution from CLI
strings.

## How it works

`OpChainSpec` wraps reth's `ChainSpec` and adds Base-specific hardfork awareness via the
`BaseUpgrades` trait. Each network spec is constructed from a bundled genesis JSON file and a
hardfork schedule:

- `BASE_MAINNET` - Base Mainnet (chain ID 8453), with London and Canyon variable base fee params.
- `BASE_SEPOLIA` - Base Sepolia testnet (chain ID 84532), with equivalent hardfork schedule.
- `BASE_DEV` - Local dev chain with all hardforks active at genesis, prefunded test accounts.

The genesis header is derived at startup from the genesis JSON using `make_op_genesis_header`,
which computes the correct state root, storage root, and other fields for Base.

Chain names are resolved from CLI strings via `SUPPORTED_CHAINS`, which maps `"base"`,
`"base_sepolia"`, `"base-sepolia"`, and `"dev"` to the corresponding static spec.

### Base fee computation

Two helpers handle hardfork-specific base fee logic:

- `decode_holocene_base_fee` - Reads the EIP-1559 elasticity and denominator packed into the
  parent block's `extra_data` field (per the Holocene spec). If both are zero, falls back to the
  chain spec's default params for that timestamp.
- `compute_jovian_base_fee` - Extends Holocene logic with a minimum base fee floor also encoded in
  `extra_data`, and uses `max(gas_used, blob_gas_used)` as the effective gas used for the
  next-block fee calculation.

Gas limits and other chain parameters are sourced from
[`base_alloy_chains::BaseChainConfig`](../../../crates/common/chains/src/config.rs).

## Usage

Add the crate to your `Cargo.toml`:

```toml,ignore
base-execution-chainspec = { workspace = true }
```

Access the pre-built chain specs:

```rust,ignore
use base_execution_chainspec::{BASE_DEV, BASE_MAINNET, BASE_SEPOLIA};

let spec = BASE_MAINNET.clone();
println!("chain: {}", spec.chain());
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
