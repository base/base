# `base-shadow-metrics`

Polling reader that turns persisted shadow blocks into Prometheus metrics.

## Overview

`ShadowMetricsReader` polls `shadow_blocks` for shadow candidate blocks that
were reorged out and emits gas used, transaction count, priority fee
inversions, empty blocks, reverted blocks, and the latest block number.
`ShadowMetricsStore` is the thin Postgres handle underneath it: it establishes
an eager `PgPool` from a connection URL and exposes a schema-readiness check for
Kubernetes-style `/readyz` probes. `base-shadow-indexer-db` owns and applies the
shared schema.

Emission happens before the cursor is persisted, making delivery at-least-once.
`ShadowMetricsReader::run` never returns an error: undecodable payloads are
counted and skipped, and database errors leave the cursor untouched so the next
tick retries the same batch.

## Usage

Metric emission is behind the `metrics` feature. Without it, `define_metrics!`
expands to no-ops and the reader runs while emitting nothing, so any consumer
that expects metrics must enable it explicitly:

```toml
[dependencies]
base-shadow-metrics = { workspace = true, features = ["metrics"] }
```

```rust,ignore
use base_shadow_metrics::{ShadowMetricsReader, ShadowMetricsReaderConfig, ShadowMetricsStore};

let store = ShadowMetricsStore::connect(&database_url, 10).await?;
store.check_schema_ready().await?;

let reader = ShadowMetricsReader::new(store, ShadowMetricsReaderConfig::default()).await?;
reader.run().await?;
```

`ShadowMetricsReaderConfig::default()` uses `DEFAULT_POLL_INTERVAL_SECS` (2) and
`DEFAULT_MAX_ROWS_PER_POLL` (1000). The poll interval tracks a writer that
flushes every second with reconciliation arriving in bursts roughly every ten
seconds; the row cap bounds catch-up so a long outage drains steadily instead of
loading every JSONB payload at once.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
