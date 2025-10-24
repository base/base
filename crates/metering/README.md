# Transaction Metering RPC

RPC endpoints for simulating and metering transaction bundles on Optimism.

## `base_meterBundle`

Simulates a bundle of transactions, providing gas usage and execution time metrics. Designed as an extension of `eth_callBundle` with additional resource metering.

**Parameters:**
- `txs`: Array of signed, RLP-encoded transactions (hex strings with 0x prefix)
- `blockNumber`: Target block number for bundle validity (hex, "latest", "pending")
- `stateBlockNumber`: Block number to base simulation on (hex, "latest", "pending")
- `timestamp` (optional): Custom timestamp (defaults to parent + 2s)

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
    "blockNumber": "0x1a2b3c",
    "stateBlockNumber": "0x1a2b3b"
  }]
}
```

## Implementation

- Executes transactions sequentially using Optimism EVM configuration
- Tracks microsecond-precision execution time per transaction
- Stops on first failure
- Automatically registered in `base` namespace

