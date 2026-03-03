# Mempool Propagation Pipeline

Multi-PR implementation plan for forwarding transactions from mempool nodes
to builder nodes via a custom RPC, replacing P2P propagation.

## Architecture

```
┌──────────────────┐   broadcast   ┌──────────────────┐     RPC      ┌──────────────────┐
│   Consumer       │──────────────▶│   Forwarder (×N) │─────────────▶│   Builder        │
│   (PR 1)         │   channel     │   (PR 2)         │   batched    │   (PR 3)         │
└──────────────────┘               └──────────────────┘              └──────────────────┘
        │                                   │                                │
        │ reads from                        │ batches txs                    │ receives via
        │ best_transactions()               │ sends via RPC                  │ base_insertValidatedTransactions
        ▼                                   ▼                                ▼
┌──────────────────┐               ┌──────────────────┐              ┌──────────────────┐
│ Transaction Pool │               │ Builder RPC URL  │              │ Builder Mempool  │
│ (timestamp order)│               │                  │              │                  │
└──────────────────┘               └──────────────────┘              └──────────────────┘
```

Mempool nodes use `TimestampOrdering` (FIFO) for the consumer iterator.
Builder nodes use `CoinbaseTipOrdering` (priority fee) for block building.

---

## PR 1: Consumer Task ✅

**Crate:** `crates/txpool/`
**Directory:** `src/consumer/`

Background task on a dedicated blocking thread that continuously drains
`best_transactions()`, validates/deduplicates, and broadcasts to subscribers.

### Files

| File | Purpose |
|------|---------|
| `consumer/mod.rs` | Module root, `run_consumer()` entry point, `ConsumerHandle` (broadcast sender) |
| `consumer/config.rs` | `ConsumerConfig` with `resend_after`, `channel_capacity`, `poll_interval` |
| `consumer/metrics.rs` | `ConsumerMetrics` — Prometheus counters/gauge |
| `consumer/validator.rs` | `RecentlySent` hash-based dedup with periodic pruning |
| `consumer/task.rs` | `Consumer<P>` struct and blocking `run()` loop |

### Configuration Defaults

| Setting | Default | Description |
|---------|---------|-------------|
| `resend_after` | 4s (2 blocks) | Skip txs sent within this window |
| `channel_capacity` | 10,000 | Bounded channel size |
| `poll_interval` | 10ms | Sleep when no new transactions sent |

### Metrics

| Metric | Type | Description |
|--------|------|-------------|
| `txpool.consumer.txs_read` | Counter | Total txs read from iterator |
| `txpool.consumer.txs_sent` | Counter | Total txs broadcast |
| `txpool.consumer.txs_ignored` | Counter | Total txs skipped (validator) |
| `txpool.consumer.dedup_cache_size` | Gauge | Current dedup cache entries |

---

## PR 2: Forwarder Task

**Crate:** `crates/txpool/`
**Directory:** `src/forwarder/`

Subscribes to the consumer's broadcast channel, batches transactions, and
sends to a builder node via a custom RPC endpoint. One forwarder is spawned
per builder node.

### Planned Types

| Type | Description |
|------|-------------|
| `ForwarderConfig` | `builder_urls: Vec<String>`, `batch_size`, `batch_timeout`, `max_retries`, `retry_backoff` |
| `ForwarderMetrics` | `batches_sent`, `txs_forwarded`, `rpc_errors`, `rpc_latency` |
| `ForwarderHandle` | Shutdown handle + stats |
| `Forwarder` | Main struct — async task consuming from `broadcast::Receiver` |
| `run_forwarder()` | Entry point, calls `consumer_handle.sender.subscribe()` |

### Algorithm

```
let mut receiver = consumer_handle.sender.subscribe();
loop {
    batch = collect_batch(&mut receiver, batch_size, batch_timeout).await;
    if batch.is_empty() { continue; }
    send_batch_rpc(&client, builder_url, &batch).await;  // with retries
}
```

### Configuration Defaults

| Setting | Default | Description |
|---------|---------|-------------|
| `builder_urls` | Required | Builder RPC endpoint URLs (one forwarder per URL) |
| `batch_size` | 100 | Max transactions per batch |
| `batch_timeout` | 50ms | Max wait time for batch fill |
| `max_retries` | 3 | Retry attempts per batch |
| `retry_backoff` | 100ms | Initial retry delay |

### Dependencies

- PR 1 (consumer broadcast sender)
- HTTP/RPC client (reqwest or jsonrpsee)

---

## PR 3: Builder RPC Endpoint

**Crate:** `crates/builder/core/` or new `crates/builder/rpc/`

RPC endpoint on builder nodes to receive forwarded transactions.

### RPC Method

```rust
#[rpc(server, namespace = "base")]
pub trait BaseTxApi {
    #[method(name = "insertValidatedTransactions")]
    async fn insert_validated_transactions(
        &self,
        txs: Vec<Bytes>,
    ) -> RpcResult<ReceiveTxsResponse>;
}
```

### Response

```rust
pub struct ReceiveTxsResponse {
    pub accepted: u64,
    pub rejected: u64,
    pub errors: Vec<TxRejection>,
}
```

### Dependencies

- None (can be implemented in parallel with PRs 1-2)

---

## PR 4: Node Integration

**Crate:** New `crates/client/tx-forwarding/` extension crate

Wires consumer + forwarder into the node using the `BaseNodeExtension` pattern.

### CLI Flags

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--enable-tx-forwarding` | bool | false | Enable the forwarding pipeline |
| `--builder-rpc-urls` | Vec\<String\> | Required | Builder RPC endpoints (one forwarder per URL) |
| `--tx-forwarding-resend-after-ms` | u64 | 4000 | Resend-after window in ms (default: 2 blocks) |
| `--tx-forwarding-batch-size` | usize | 100 | Forwarder batch size |
| `--tx-forwarding-batch-timeout-ms` | u64 | 50 | Batch timeout in ms |

### Extension Pattern

```rust
impl BaseNodeExtension for TxForwardingExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.add_rpc_module(move |ctx| {
            let pool = ctx.pool().clone();
            let handle = run_consumer(pool, self.consumer_config);
            // One forwarder per builder URL, each subscribing to the broadcast
            for url in &self.forwarder_config.builder_urls {
                let rx = handle.sender.subscribe();
                run_forwarder(rx, url.clone(), &self.forwarder_config);
            }
            Ok(())
        })
    }
}
```

### Registration

```rust
// bin/node/src/main.rs
if args.enable_tx_forwarding {
    runner.install_ext::<TxForwardingExtension>(config);
}
```

### Dependencies

- PR 1 (consumer)
- PR 2 (forwarder)
- PR 3 (builder RPC)

---

## Implementation Order

```
PR 1: Consumer ──────┐
                      ├──▶ PR 2: Forwarder ──┐
PR 3: Builder RPC ───┘                       ├──▶ PR 4: Node Integration
                      ────────────────────────┘
```

PRs 1 and 3 can be developed in parallel. PR 2 depends on PR 1.
PR 4 depends on all three.

## Testing Strategy

| PR | Tests |
|----|-------|
| PR 1 | Unit tests for `RecentlySent`, `ConsumerConfig`. Integration with mock pool. |
| PR 2 | Unit tests for batching. Integration with mock HTTP server. |
| PR 3 | Unit tests for tx decoding. Integration for RPC endpoint. |
| PR 4 | End-to-end: submit tx → consumer → forwarder → builder. |
