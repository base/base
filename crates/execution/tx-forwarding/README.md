# base-tx-forwarding

Transaction forwarding extension for Base node. Forwards transactions from the mempool to builder RPC endpoints.

## Overview

This crate provides:

- **Transaction Consumer**: Subscribes to pool events and broadcasts transactions to forwarders
- **Transaction Forwarder**: Batches and forwards transactions to builder RPC endpoints
- **Resend Logic**: Automatically resends transactions that haven't been included after a configurable window

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
