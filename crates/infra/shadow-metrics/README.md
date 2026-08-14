# `base-shadow-metrics`

Postgres access for the shadow metrics reader.

## Overview

Provides `ShadowMetricsStore`, a thin Postgres handle for reading shadow blocks
and persisting the metrics cursor. It establishes an eager `PgPool` from a
connection URL and exposes a schema-readiness check for Kubernetes-style
`/readyz` probes. `base-shadow-indexer-db` owns and applies the shared schema.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-shadow-metrics = { workspace = true }
```

```rust,ignore
use base_shadow_metrics::ShadowMetricsStore;

let store = ShadowMetricsStore::connect(&database_url, 10).await?;
store.check_schema_ready().await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
