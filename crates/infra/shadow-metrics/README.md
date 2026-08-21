# `base-shadow-metrics`

Polling reader that turns persisted shadow blocks into Prometheus metrics.

## Overview

`ShadowMetricsReader` polls `shadow_blocks` and emits gas used, transaction
count, priority fee inversions, empty blocks, reverted blocks, and the latest
block number. `shadow_blocks` is keyed by block number alone, so it contains
only shadow candidate blocks that were reorged out; canonical blocks are not
persisted, and every row the reader sees is therefore a reorg. A chain that has
not reorged yet leaves the table empty. `ShadowMetricsStore` is the thin
Postgres handle underneath it: it establishes an eager `PgPool` from a
connection URL and exposes a schema-readiness check for Kubernetes-style
`/readyz` probes. `base-shadow-indexer-db` owns and applies the shared schema.

Emission happens before the cursor is persisted, making delivery at-least-once.
`ShadowMetricsReader::run` never returns an error: database errors leave the
cursor untouched so the next tick retries the same batch. Payloads deserialize
during the database fetch, so one incompatible payload fails the entire poll,
increments `poll_errors_total`, and stalls the reader until that row is repaired
or deleted. This is an accepted trade-off for using one typed database row.

## Deploy order

shadow-indexer applies the migrations at startup and must roll out before
shadow-metrics. During that window an old reader queries columns the new schema
no longer has, so every poll fails and only `poll_errors_total` moves; a metrics
gap for the length of the roll is expected. Deploying in the reverse order is
also broken, and `check_schema_ready` fails `/readyz` for that case rather than
reporting healthy while the cursor silently stalls.

## Usage

Metric emission is enabled by default through the `metrics` feature:

```toml
[dependencies]
base-shadow-metrics.workspace = true
```

Setting `default-features = false` explicitly disables emission and makes the
generated metric handles no-ops.

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
