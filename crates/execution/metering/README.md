# `base-metering`

Metering RPC for Base node. Provides RPC methods for measuring transaction and block execution timing.

## Overview

Exposes JSON-RPC endpoints for profiling transaction and block execution on the Base node.
`base_meterBundle` simulates a bundle and returns per-transaction gas and timing metrics.
`base_meterBlockByHash` and `base_meterBlockByNumber` re-execute a historical block and return
a breakdown of signer recovery, EVM execution, and parent-state provider reads. Account, storage,
and bytecode fetch counts and cumulative latency are reported for the block and each transaction.
Per-transaction results also identify fetched addresses, storage keys, and code hashes so costly
state access patterns can be ranked and used to validate prefetch hints.
`base_meteredPriorityFeePerGas` combines bundle metering with a priority fee recommendation
based on recent block resource usage.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-metering = { workspace = true }
```

## RPC Methods

### `base_meterBundle`

Simulates and meters a bundle of transactions.

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

### `base_meteredPriorityFeePerGas`

Meters a bundle and returns a recommended priority fee based on recent block congestion.

**Parameters:**
- `bundle`: Bundle object containing transactions to simulate

**Returns:**
- `MeteredPriorityFeeResponse`: Contains metering results plus priority fee recommendation

**Response:**
```json
{
  "bundleGasPrice": "0x...",
  "bundleHash": "0x...",
  "results": [...],
  "totalGasUsed": 21000,
  "totalExecutionTimeUs": 1234,
  "priorityFee": "0x5f5e100",
  "blocksSampled": 12,
  "resourceEstimates": [
    {
      "resource": "gasUsed",
      "thresholdPriorityFee": "0x3b9aca00",
      "recommendedPriorityFee": "0x5f5e100",
      "cumulativeUsage": "0x1e8480",
      "thresholdTxCount": 5,
      "totalTransactions": 10
    },
    {
      "resource": "dataAvailability",
      ...
    }
  ]
}
```

**Algorithm:**
1. Meter the bundle to get resource consumption (gas and DA bytes)
2. Use cached metering data from recent blocks (populated by ingestion pipeline)
3. For each block in the cache:
   - Estimate gas and DA bytes against cumulative
     transaction prefixes for scheduled tx-pool flashblocks `1..=target_flashblocks_per_block`,
     using the same growing cumulative targets the builder derives from whole-block budgets.
   - These estimates use the configured target number of tx-pool flashblocks per block, not the
     number of flashblocks observed in the cache. The base flashblock at index `0` is not part of
     this schedule.
   - Use the block-end estimate for the accumulating resources as that block's rolling summary.
4. Take the median fee across all blocks for each resource (upper median for even counts)
5. Return the maximum fee across all resources as `priorityFee`

Note: The cache must be populated by the ingestion pipeline for estimates to be available.
The `blocksSampled` field indicates how many blocks were used in the rolling estimate.
For gas or DA estimation, `target_flashblocks_per_block` must be configured so the estimator can
mirror the builder's flashblock budgeting.

## Ingestion

The metering collector consumes `PendingBlocks` flashblock snapshots and stores transaction
resource usage in the metering cache. It retains execution timing for per-transaction execution
limits, but priority-fee estimation only uses gas and DA resources.

## Architecture

The ingestion pipeline works as follows:

1. The flashblocks websocket feed updates `PendingBlocks` snapshots for the current pending range
2. `MeteringCollector` walks newly observed flashblocks from those snapshots
3. DA bytes are computed from the raw transaction bytes in each flashblock diff
4. Transactions are inserted into `MeteringCache` at the correct block/flashblock location
5. `base_meteredPriorityFeePerGas` uses the cache to estimate gas and DA priority fees

Note: flashblock diffs must include raw transaction bytes for accurate DA-based priority fee
estimation. These bytes are used to compute compressed transaction size via `flz_compress_len`.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
