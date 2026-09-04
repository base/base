# `base-metering`

Metering RPC for Base node. Provides RPC methods for measuring transaction and block execution timing.

## Overview

Exposes JSON-RPC endpoints for profiling transaction and block execution on the Base node.
`base_meterBundle` simulates a bundle against latest canonical state and returns
per-transaction gas, opcode, and timing metrics.
`base_meterBlockByHash` and `base_meterBlockByNumber` re-execute a historical block and return
a breakdown of signer recovery and EVM execution times.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-metering = { workspace = true }
```

## RPC Methods

### `base_meterBundle`

Simulates and meters a bundle of transactions against latest canonical state.

**Parameters:**
- `bundle`: Bundle object containing transactions to simulate

**Returns:**
- `MeterBundleResponse`: Contains per-transaction results, total gas used, execution times

#### Pseudo-opcode vocabulary

The CLI/RPC `meteredOpcodes` filter accepts the following transaction-level pseudo-opcodes.
These strings are also returned in each `OpcodeGas` entry in `base_meterBundle`
responses; the rename is breaking and there are no legacy aliases.

- `INTRINSIC_TOTAL`: the active schedule's additive intrinsic gas.
- EIP-2028/EIP-7623 data buckets: `INTRINSIC_TX_DATA_ZERO_BYTE_COST` and
  `INTRINSIC_TX_DATA_NON_ZERO_BYTE_COST`.
- EIP-2930 prepaid access-list buckets: `INTRINSIC_ACCESS_LIST_ADDRESS_COST` and
  `INTRINSIC_ACCESS_LIST_STORAGE_KEY_COST`. These are access-list entries, not
  EIP-7928 block-level access-list (BAL) observations.
- EIP-3860 initcode: `INTRINSIC_INITCODE_WORD_COST`.
- EIP-7623 floor candidate: `TX_FLOOR_GAS`, reported separately from
  `INTRINSIC_TOTAL`.
- Legacy pre-Amsterdam aggregates: `INTRINSIC_LEGACY_TX_BASE_COST`,
  `INTRINSIC_LEGACY_CREATE_COST`, and the EIP-7702
  `INTRINSIC_PER_EMPTY_ACCOUNT_COST`.
- EIP-2780 resource primitives, emitted only when the active execution schedule
  implements that decomposition: `INTRINSIC_TX_BASE_COST`,
  `INTRINSIC_COLD_ACCOUNT_ACCESS`, `INTRINSIC_TX_VALUE_COST`,
  `INTRINSIC_TRANSFER_LOG_COST`, `INTRINSIC_CREATE_ACCESS`, and
  `INTRINSIC_REGULAR_PER_AUTH_BASE_COST`.
- EIP-2780/EIP-7708 ETH-transfer effects:
  `TX_EFFECT_ETH_TRANSFER_TO_NONEXISTENT_ACCOUNT`,
  `TX_EFFECT_ETH_TRANSFER_TO_EXISTING_ACCOUNT`, and
  `TX_EFFECT_ETH_SELF_TRANSFER`. These are zero-gas classifiers, not intrinsic
  buckets.
- Net post-state effects from post-tx `EvmState`, counted from `original → present`
  rather than journal size: `STATE_NEW_STORAGE_SLOT`,
  `STATE_CHANGED_STORAGE_SLOT`, `STATE_CLEARED_STORAGE_SLOT`,
  `STATE_TOUCHED_ACCOUNT`, and `STATE_CHANGED_ACCOUNT`. These are zero-gas
  counts, not `SSTORE`. `STATE_CHANGED_STORAGE_SLOT` is a superset of new slots
  and clears. Loaded but unwritten accounts and slots are omitted.

Standard-transaction values are calculated from the active revm gas schedule,
including calldata, creation, initcode, access-list, authorization, and floor
costs. EIP-2780 names are not emitted with legacy values on Base schedules that
do not implement EIP-2780. EIP-7928 BAL is a separate block-level artifact and
is intentionally not represented as `OpcodeGas`.

### `base_meterBlockByHash`

Re-executes a block by hash and returns timing metrics.

**Parameters:**
- `hash`: Block hash (B256)

**Returns:**
- `MeterBlockResponse`: Contains timing breakdown for signer recovery and EVM execution

### `base_meterBlockByNumber`

Re-executes a block by number and returns timing metrics.

**Parameters:**
- `number`: Block number or tag (e.g., "latest")

**Returns:**
- `MeterBlockResponse`: Contains timing breakdown for signer recovery and EVM execution

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
