# `base-shadow-metrics`

Postgres connectivity for the shadow-metrics noop mock service.

## Overview

Provides `ShadowMetricsSink`, a thin Postgres handle modeled on the audit
archiver's transaction-event sink. It establishes an eager `PgPool` from a
connection URL, embeds sqlx migrations, and exposes a schema-readiness check for
Kubernetes-style `/readyz` probes. The service itself performs no real work; it
is a scaffold that proves configuration-driven Postgres connectivity.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-shadow-metrics = { workspace = true }
```

```rust,ignore
use base_shadow_metrics::ShadowMetricsSink;

// Run migrations, then connect.
ShadowMetricsSink::migrate(&database_url).await?;
let sink = ShadowMetricsSink::connect(&database_url, 10).await?;
sink.check_schema_ready().await?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
