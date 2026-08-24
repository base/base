# `base-shadow-metrics`

Polling reader that turns persisted shadow blocks into Prometheus metrics.

## Overview

`ShadowMetricsReader` polls `shadow_blocks` for shadow candidate blocks that
were reorged out and emits gas used, transaction count, priority fee
inversions, empty blocks, and the latest block number.
`ShadowMetricsStore` is the thin Postgres handle underneath it: it establishes
an eager `PgPool` from a connection URL and exposes a schema-readiness check for
Kubernetes-style `/readyz` probes. `base-shadow-indexer-db` owns and applies the
shared schema.

A row is emitted only once `canonical_hash` is set. A reorg records its
displaced blocks before the chain has produced every replacement, so rows
arrive unresolved; the reader advances past them and picks them up when the
indexer fills the hash in, which bumps `updated_at`. A row that never gains a
canonical hash is never emitted: nothing distinguishes it from one still
awaiting its replacement.

`shadow_blocks` is keyed by `number`, so a second reorg at a height overwrites
the candidate stored there. A candidate replaced before this reader polled it is
never emitted, which makes `blocks_inspected_total` a lower bound rather than an
exact count of discarded blocks.

Emission happens before the cursor is persisted, making delivery at-least-once.
`ShadowMetricsReader::run` never returns an error: database errors leave the
cursor untouched so the next tick retries the same batch. Payloads deserialize
during the database fetch, so one incompatible payload fails the entire poll,
increments `poll_errors_total`, and stalls the reader until that row is repaired
or deleted. This is an accepted trade-off for using one typed database row.

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
