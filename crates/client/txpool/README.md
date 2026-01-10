# `base-txpool`

<a href="https://github.com/base/node-reth/actions/workflows/ci.yml"><img src="https://github.com/base/node-reth/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/node-reth/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Base-specific transaction pool extensions for `reth`, including transaction tracing and RPC endpoints.

## Overview

This crate provides:

- **Transaction Tracing ExEx**: An execution extension that subscribes to mempool events and chain notifications to track how long a transaction spends in each stage of the lifecycle before it is included, dropped, or replaced.
- **Transaction Status RPC**: An RPC API (`txpool_transactionStatus`) to query the current status and lifecycle events of a transaction by hash.
- **Tracker**: Core tracking logic for recording pending/queued transitions, replacements, drops, and block inclusion.
- **Metrics**: Histogram metrics for mempool residency by event type to help spot latency regressions.

## Usage

Enable transaction tracing on the Base node CLI:

```bash
cargo run -p node --release -- \
  --enable-transaction-tracing \
  --enable-transaction-tracing-logs  # optional: emit per-tx lifecycle logs
```

From code, wire the ExEx into the node builder:

```rust,ignore
use base_client_runner::{TracingConfig, extensions::TransactionTracingExtension};

let tracing = TracingConfig { enabled: true, logs_enabled: true };
let builder = TransactionTracingExtension::new(tracing).apply(builder);
```

## Metrics

The extension records a histogram named `reth_transaction_tracing_tx_event` with an `event` label for each lifecycle event (`pending`, `queued`, `replaced`, `dropped`, `block_inclusion`, etc.). Values represent the milliseconds a transaction spent in the mempool up to that event. When the in-memory log reaches its limit (20,000 transactions), an `overflowed` event is recorded so dashboards can alert on data loss.
