# `base-metering`

Metering RPC for Base node. Provides RPC methods for measuring transaction and block execution timing.

## Overview

Exposes JSON-RPC endpoints for profiling transaction and block execution on the Base node.
`base_meterBundle` simulates a bundle and returns per-transaction gas and timing metrics.
`base_meterBlockByHash` and `base_meterBlockByNumber` re-execute a historical block and return
a breakdown of signer recovery, EVM execution, and state root computation times.
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

### `base_meterBlockByHash`

Re-executes a block by hash and returns timing metrics.

**Parameters:**
- `hash`: Block hash (B256)

**Returns:**
- `MeterBlockResponse`: Contains timing breakdown for signer recovery, EVM execution, and state root calculation

### `base_meterBlockByNumber`

Re-executes a block by number and returns timing metrics.

**Parameters:**
- `number`: Block number or tag (e.g., "latest")

**Returns:**
- `MeterBlockResponse`: Contains timing breakdown for signer recovery, EVM execution, and state root calculation

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
      "resource": "executionTime",
      ...
    },
    {
      "resource": "dataAvailability",
      ...
    }
  ]
}
```

**Algorithm:**
1. Meter the bundle to get resource consumption (gas, execution time, DA bytes)
2. Use cached metering data from recent blocks (populated by ingestion pipeline)
3. For each block in the cache:
   - For execution time, estimate each tx-pool flashblock independently because that budget
     resets each flashblock in the builder
   - For gas, state root time, and DA bytes, estimate against cumulative transaction prefixes
     for scheduled tx-pool flashblocks `1..=target_flashblocks_per_block`, mirroring the
     builder's per-flashblock cumulative targets and excluding the base flashblock at index `0`
4. Take the median fee across all blocks for each resource (upper median for even counts)
5. Return the maximum fee across all resources as `priorityFee`

Note: The cache must be populated by the ingestion pipeline for estimates to be available.
The `blocksSampled` field indicates how many blocks were used in the rolling estimate.
For gas, state root time, or DA estimation, `target_flashblocks_per_block` must be configured so
the estimator can mirror the builder's flashblock budgeting.

## Ingestion RPCs

These RPC methods are used to populate the metering cache with transaction resource usage data.
They are called by an external ingestion pipeline (e.g., tips-ingress) that meters transactions
as they are processed.

### `base_setMeteringInfo`

Sets metering information for a transaction.

**Parameters:**
- `tx_hash`: Transaction hash (B256)
- `meter`: `MeterBundleResponse` containing resource usage data

**Returns:**
- `()`: Empty success response

This method stores metering data in a pending map. When a flashblock inclusion event is received
(via websocket), the annotator correlates the pending data with the actual block/flashblock
location and inserts it into the cache.

### `base_setMeteringEnabled`

Enables or disables metering data collection.

**Parameters:**
- `enabled`: Boolean indicating whether metering should be enabled

**Returns:**
- `()`: Empty success response

### `base_clearMeteringInfo`

Clears all pending metering information.

**Parameters:** None

**Returns:**
- `()`: Empty success response

## Architecture

The ingestion pipeline works as follows:

1. External service (tips-ingress) meters transactions and calls `base_setMeteringInfo`
2. Metering data is stored in a pending map indexed by transaction hash
3. Flashblock websocket feed sends `FlashblockInclusion` events with (block, flashblock, transactions)
4. `ResourceAnnotator` correlates pending data with flashblock inclusions
5. DA bytes are computed from the raw transaction bytes in the `FlashblockInclusion`
6. Matched transactions are inserted into `MeteringCache` at the correct location
7. `base_meteredPriorityFeePerGas` uses the cache to estimate priority fees

Note: The `FlashblockInclusion` must include raw transaction bytes (`IncludedTransaction.raw_tx`)
for accurate DA-based priority fee estimation. These bytes are used to compute the compressed
transaction size via `flz_compress_len`.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
