# `base-shadow-metrics`

Read-only HTTP JSON API over persisted shadow blocks.

## Overview

`api_router` serves reorged-out shadow candidate blocks to the block-explorer
UI: block detail by hash, and the shadow candidates a canonical block replaced.
`ShadowMetricsStore` is the thin Postgres handle underneath it: it establishes
an eager `PgPool` from connection parameters and exposes a schema-readiness check
for Kubernetes-style `/readyz` probes. `base-shadow-indexer-db` owns and applies
the shared schema and the queries the API runs.

A shadow block gains a `canonical_hash` once the block that replaced it at its
height becomes canonical. `shadow_blocks` is keyed by `number`, so a second
reorg at a height overwrites the candidate stored there. `ShadowBlockStats`
derives per-block figures (gas used, transaction count, priority-fee inversions)
from a stored row for the summary endpoints.

## Usage

```toml
[dependencies]
base-shadow-metrics.workspace = true
```

```rust,ignore
use base_shadow_metrics::{ShadowMetricsStore, api_router};

let store = ShadowMetricsStore::connect(&connection, 10).await?;
store.check_schema_ready().await?;

let app = api_router(Some(store));
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
