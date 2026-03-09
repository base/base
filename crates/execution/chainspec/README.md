# `base-execution-chainspec`

Chainspec types for Base execution.

This crate provides `OpChainSpec`, the chain specification type used by Base nodes, along with
pre-built specs for Base Mainnet, Base Sepolia, and a local dev chain. It also includes base fee
computation helpers for Holocene and Jovian hardforks.

## How it works

`OpChainSpec` wraps reth's `ChainSpec` and adds Base-specific hardfork awareness via the
`OpHardforks` trait. Each network spec is constructed from a bundled genesis JSON file and a
hardfork schedule:

- `BASE_MAINNET` - Base Mainnet (chain ID 8453), with London and Canyon variable base fee params.
- `BASE_SEPOLIA` - Base Sepolia testnet (chain ID 84532), with equivalent hardfork schedule.
- `OP_DEV` - Local dev chain with all hardforks active at genesis, prefunded test accounts.

The genesis header is derived at startup from the genesis JSON using `make_op_genesis_header`,
which computes the correct state root, storage root, and other fields for Base.

Chain names are resolved from CLI strings via `parse_op_chain_spec`, which maps `"base"`,
`"base_sepolia"`, `"base-sepolia"`, and `"dev"` to the corresponding static spec.

### Base fee computation

Two helpers handle hardfork-specific base fee logic:

- `decode_holocene_base_fee` - Reads the EIP-1559 elasticity and denominator packed into the
  parent block's `extra_data` field (per the Holocene spec). If both are zero, falls back to the
  chain spec's default params for that timestamp.
- `compute_jovian_base_fee` - Extends Holocene logic with a minimum base fee floor also encoded in
  `extra_data`, and uses `max(gas_used, blob_gas_used)` as the effective gas used for the
  next-block fee calculation.

### Constants

The `constants` module exposes gas limit maximums observed on-chain:

- `BASE_MAINNET_MAX_GAS_LIMIT` - 105,000,000
- `BASE_SEPOLIA_MAX_GAS_LIMIT` - 45,000,000

## Usage

Add the crate to your `Cargo.toml`:

    base-execution-chainspec = { workspace = true }

Access the pre-built chain specs:

    use base_execution_chainspec::{BASE_MAINNET, BASE_SEPOLIA, OP_DEV};

    let spec = BASE_MAINNET.clone();
    println!("chain: {}", spec.chain());

Parse a chain spec from a CLI argument:

    use base_execution_chainspec::parse_op_chain_spec;

    let spec = parse_op_chain_spec("base").expect("unknown chain");

Compute the next base fee under Holocene rules:

    use base_execution_chainspec::decode_holocene_base_fee;

    let base_fee = decode_holocene_base_fee(&chain_spec, &parent_header, next_timestamp)?;

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
