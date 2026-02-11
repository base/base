# base-metering

Metering RPC for Base node. Provides RPC methods for measuring transaction and block execution timing.

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
   - For each flashblock within the block:
     - For each resource type, run the estimation algorithm:
       - Walk from highest-paying transactions, subtracting usage from remaining capacity
       - Stop when adding another tx would leave less room than the bundle needs
       - The last included tx's fee is the threshold
     - For "use-it-or-lose-it" resources (execution time), aggregate usage across all flashblocks
   - Take the maximum fee across all flashblocks for each resource
4. Take the median fee across all blocks for each resource (upper median for even counts)
5. Return the maximum fee across all resources as `priorityFee`

Note: The cache must be populated by the ingestion pipeline for estimates to be available.
The `blocksSampled` field indicates how many blocks were used in the rolling estimate.

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

## Dynamic DA Size Configuration

For accurate priority fee estimates, the estimator needs to know the sequencer's current DA
limit. Since sequencers dynamically adjust their DA budget via `miner_setMaxDASize` based on
L1 gas prices and network conditions, the estimator must read from the same shared config.

When `OpDAConfig` is provided to the `PriorityFeeEstimator`, it reads `max_da_block_size`
from this shared config on each estimation request. This ensures fee estimates reflect the
actual DA capacity the sequencer is using, not a stale hardcoded value.

The static `data_availability_bytes` in `ResourceLimits` serves as a fallback when
`OpDAConfig` is not provided, but this configuration is not recommended for production use.
