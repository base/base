# Transaction Tracing ExEx for OP Stack

A Reth execution extension (ExEx) that subscribes to flashblocks events for the OP Stack and tracks transaction timing, with specific monitoring for address `0x889766967Dd3FF6A0C91b799322D45628e68F8b1`.

## Features

- **Real-time Transaction Monitoring**: Subscribes to flashblocks events to track transactions as they appear in blocks
- **Transaction Timing**: Measures the time between first detection and block inclusion
- **Address-Specific Monitoring**: Tracks balance changes for the monitored address `0x889766967Dd3FF6A0C91b799322D45628e68F8b1`
- **Comprehensive Logging**: Provides detailed logs for all transaction events and timing metrics
- **Automatic Cleanup**: Removes old transaction traces to prevent memory bloat

## Architecture

This implementation leverages the existing flashblocks infrastructure in the codebase:

- **FlashblocksSubscriber**: Connects to a WebSocket endpoint to receive real-time flashblock data
- **FlashblocksState**: Manages transaction state and provides APIs for querying transaction details
- **TransactionTracer**: Core component that processes flashblocks and tracks transaction timing

## Usage

### Building

```bash
cd crates/transaction-tracing
cargo build --release
```

### Running

The transaction tracer requires a WebSocket URL to connect to the flashblocks stream:

```bash
./target/release/transaction-tracing --websocket-url ws://your-flashblocks-endpoint
```

### Command Line Options

- `--websocket-url`: WebSocket URL for flashblocks data stream (required for tracing)
- Standard Reth/OP Stack options are also available through the base CLI

### Example Output

When running, the tracer will output logs like:

```
INFO starting transaction tracing node for OP Stack
INFO Starting transaction tracing with flashblocks integration
INFO Transaction tracing started for address: 0x889766967dd3ff6a0c91b799322d45628e68f8b1
INFO Starting transaction monitoring for address: 0x889766967dd3ff6a0c91b799322d45628e68f8b1
INFO Processing flashblock 0 with 5 transactions in block 12345
INFO New transaction 0xabc123... detected in flashblock 0 for block 12345
INFO Transaction 0xabc123... included in block 12345 after 150ms (150ms)
INFO Monitored address 0x889766967dd3ff6a0c91b799322d45628e68f8b1 balance changed to 1000000000000000000 in block 12345
```

## Configuration

### Monitored Address

The monitored address is currently hardcoded as:
```rust
const MONITORED_ADDRESS: &str = "0x889766967Dd3FF6A0C91b799322D45628e68F8b1";
```

To monitor a different address, modify this constant and rebuild.

### Cleanup Settings

Transaction traces are automatically cleaned up after 5 minutes:
```rust
const MAX_AGE: Duration = Duration::from_secs(300); // 5 minutes
```

## Technical Details

### Transaction Timing Metrics

The tracer measures:
- **First Seen Timestamp**: When the transaction was first detected in a flashblock
- **Inclusion Timestamp**: When the transaction was included in a block
- **Time to Inclusion**: Duration between first detection and block inclusion

### Data Structures

```rust
struct TransactionTrace {
    tx_hash: TxHash,
    timestamp_first_seen: SystemTime,
    block_number: Option<u64>,
    timestamp_included: Option<SystemTime>,
}
```

### Integration with Flashblocks

The tracer integrates with the existing flashblocks infrastructure:
- Uses `FlashblocksSubscriber` to receive real-time data
- Leverages `FlashblocksState` for transaction queries
- Monitors both transaction receipts and account balance changes

## Dependencies

This crate depends on:
- `base-reth-flashblocks-rpc`: For flashblocks integration
- `reth-optimism-*`: For OP Stack support
- `alloy-*`: For Ethereum primitives and types
- Standard Rust async ecosystem (`tokio`, etc.)

## Limitations

1. **Address Detection**: Currently monitors balance changes rather than parsing transaction sender/receiver fields due to complexity of transaction envelope parsing
2. **WebSocket Dependency**: Requires a working flashblocks WebSocket endpoint
3. **Memory Usage**: Keeps transaction traces in memory for 5 minutes

## Future Enhancements

- Support for multiple monitored addresses
- More sophisticated transaction field parsing to detect sender/receiver
- Persistent storage of timing metrics
- Configurable cleanup intervals
- Metrics exposition for monitoring systems