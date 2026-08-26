# base-tx-forwarding

Transaction forwarding service and node extension for Base. Forwards transactions from the mempool to builder RPC endpoints.

## Overview

This crate provides:

- **`TxForwardingService`**: Starts one reader and forwarder pipeline per destination
- **`TxForwardingHandle`**: Gracefully stops readers and drains destination queues
- **Per-destination delivery**: Each builder has an independent bounded queue and deduplication cache
- **Resend logic**: Automatically resends transactions that haven't been included after a configurable window

A slow builder backpressures only its own reader. Transactions are marked as recently sent only
after that builder's queue accepts them, so another destination cannot suppress their delivery.
Each forwarder sends an isolated request immediately, but drains any other requests already waiting
for that destination into the same RPC batch up to the configured batch size.

## Forwarding without the pool

`TxForwardingService::spawn_requests` starts the same transport — batching, rate limiting, retries,
metrics and shutdown — driven by queues the caller owns rather than by draining a `TransactionPool`.
Use it when requests arrive by push, or when a producer needs its own overflow policy.

A request is anything implementing `ForwardRequest`, which supplies a per-call method name and
params. Because the method is chosen per request, one destination queue may carry several kinds of
call, and a batch preserves submission order between them:

```rust,ignore
enum Message {
    Insert(Box<ValidatedTransaction<MyExtensions>>),
    Remove(TxHash),
}

impl ForwardRequest for Message {
    fn method(&self) -> &'static str {
        match self {
            Self::Insert(_) => "base_insertValidatedTransaction",
            Self::Remove(_) => "my_removeTransaction",
        }
    }
    // ...
}

let (sender, receiver) = tokio::sync::mpsc::channel(1024);
let handle = TxForwardingService::new(config).spawn_requests(vec![(url, receiver)], &executor)?;
```

Unlike `spawn`, an endpoint that cannot be turned into a client is a `ForwardingSetupError` rather
than a logged skip, so a caller never silently forwards to fewer destinations than it asked for.

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
cargo run -p base-reth-node --release -- \
  --enable-tx-forwarding \
  --builder-rpc-urls http://builder1:8545,http://builder2:8545 \
```
