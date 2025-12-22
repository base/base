## Base Reth Node RPC

This crate provides all the extensions and modifications
to the RPCs for Base Reth Nodes.

### Base

#### `base_meterBundle`

Simulates a bundle of transactions, providing gas usage and execution time metrics. The response format is derived from `eth_callBundle`, but the request uses the [TIPS Bundle format](https://github.com/base/tips) to support TIPS's additional bundle features.

**Parameters:**

The method accepts a Bundle object with the following fields:

- `txs`: Array of signed, RLP-encoded transactions (hex strings with 0x prefix)
- `blockNumber`: Block number for simulation state (falls back to 'latest' if invalid or not provided)
- `minTimestamp` (optional): Minimum timestamp for bundle validity (also used as simulation timestamp if provided)
- `maxTimestamp` (optional): Maximum timestamp for bundle validity
- `revertingTxHashes` (optional): Array of transaction hashes allowed to revert
- `replacementUuid` (optional): UUID for bundle replacement
- `flashblockNumberMin` (optional): Minimum flashblock number constraint
- `flashblockNumberMax` (optional): Maximum flashblock number constraint
- `droppingTxHashes` (optional): Transaction hashes to exclude from bundle

**Returns:**
- `bundleGasPrice`: Average gas price
- `bundleHash`: Bundle identifier
- `coinbaseDiff`: Total gas fees paid
- `ethSentToCoinbase`: ETH sent directly to coinbase
- `gasFees`: Total gas fees
- `stateBlockNumber`: Actual block used for simulation (may differ from requested blockNumber if fallback applied)
- `requestedBlockNumber`: The blockNumber from request (for logging/debugging)
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
