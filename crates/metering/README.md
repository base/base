# Transaction Metering RPC

RPC endpoints for simulating and metering transaction bundles on Optimism.

## `base_meterBundle`

Simulates a bundle of transactions, providing gas usage and execution time metrics. The response format is derived from `eth_callBundle`, but the request uses the [TIPS Bundle format](https://github.com/base/tips) to support TIPS's additional bundle features.

**Parameters:**

The method accepts a Bundle object with the following fields:

- `txs`: Array of signed, RLP-encoded transactions (hex strings with 0x prefix)
- `block_number`: Target block number (used both for bundle validity and as the state block for simulation)
- `min_timestamp` (optional): Minimum timestamp for bundle validity (also used as simulation timestamp if provided)
- `max_timestamp` (optional): Maximum timestamp for bundle validity
- `reverting_tx_hashes` (optional): Array of transaction hashes allowed to revert
- `replacement_uuid` (optional): UUID for bundle replacement
- `flashblock_number_min` (optional): Minimum flashblock number constraint
- `flashblock_number_max` (optional): Maximum flashblock number constraint
- `dropping_tx_hashes` (optional): Transaction hashes to exclude from bundle

**Returns:**
- `bundleGasPrice`: Average gas price
- `bundleHash`: Bundle identifier
- `coinbaseDiff`: Total gas fees paid
- `ethSentToCoinbase`: ETH sent directly to coinbase
- `gasFees`: Total gas fees
- `stateBlockNumber`: Block number used for state
- `totalGasUsed`: Total gas consumed
- `totalExecutionTimeUs`: Total execution time (μs)
- `results`: Array of per-transaction results:
  - `txHash`, `fromAddress`, `toAddress`, `value`
  - `gasUsed`, `gasPrice`, `gasFees`, `coinbaseDiff`
  - `ethSentToCoinbase`: Always "0" currently
  - `executionTimeUs`: Transaction execution time (μs)

**Example:**

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "base_meterBundle",
  "params": [{
    "txs": ["0x02f8...", "0x02f9..."],
    "blockNumber": 1748028,
    "minTimestamp": 1234567890,
    "revertingTxHashes": []
  }]
}
```

Note: While some fields like `revertingTxHashes` are part of the TIPS Bundle format, they are currently ignored during simulation. The metering focuses on gas usage and execution time measurement.

## Implementation

- Executes transactions sequentially using Optimism EVM configuration
- Tracks microsecond-precision execution time per transaction
- Stops on first failure
- Automatically registered in `base` namespace

