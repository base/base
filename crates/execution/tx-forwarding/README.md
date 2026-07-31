# base-tx-forwarding

Transaction forwarding service and node extension for Base. Forwards transactions from the mempool to builder RPC endpoints.

## Overview

This crate provides:

- **`TxForwardingService`**: Starts one consumer and forwarder pipeline per destination
- **`TxForwardingHandle`**: Gracefully stops consumers and drains destination queues
- **Per-destination delivery**: Each builder has an independent bounded queue and deduplication cache
- **Resend logic**: Automatically resends transactions that haven't been included after a configurable window

A slow builder backpressures only its own consumer. Transactions are marked as recently sent only
after that builder's queue accepts them, so another destination cannot suppress their delivery.

## CLI Flags

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--enable-tx-forwarding` | bool | false | Enable the forwarding pipeline |
| `--builder-rpc-urls` | Vec<Url> | Required | Builder RPC endpoints (one forwarder per URL) |
| `--tx-forwarding-resend-after-ms` | u64 | 4000 | Resend-after window in ms (default: 2 blocks) |
| `--tx-forwarding-batch-size` | usize | 100 | Forwarder batch size |
| `--tx-forwarding-max-rps` | u32 | 200 | Maximum RPC requests per second per forwarder |

## Usage

Enable transaction forwarding on the Base node CLI:

```bash
cargo run -p node --release -- \
  --enable-tx-forwarding \
  --builder-rpc-urls http://builder1:8545,http://builder2:8545 \
```
