# `shadow-metrics`

Service process for shadow block metrics. It polls Postgres for shadow
candidate blocks that were reorged out, emits Prometheus metrics about them,
and exposes Kubernetes-style health probes.

## Behavior

On startup, the service connects to Postgres when
`SHADOW_METRICS_POSTGRES_URL` is set, resolves the reader's starting cursor,
spawns the poll loop, and starts an HTTP health server (`GET /healthz`, `GET
/readyz`).

Each poll reads shadow block rows newer than the persisted cursor and emits
gas used, transaction count, priority fee inversions, empty blocks, and the
latest block number. Rows whose canonical hash is not yet known are skipped
and revisited once `shadow-indexer` resolves them. Metrics are emitted before
the cursor is persisted, so delivery is at-least-once. Payloads deserialize during the database
fetch, so one incompatible payload fails the entire poll, increments
`poll_errors_total`, and leaves the cursor untouched. This accepted trade-off
stalls the reader on the same batch until the offending row is repaired or
deleted; other database errors have the same retry behavior.

When `SHADOW_METRICS_POSTGRES_URL` is unset, Postgres connectivity is disabled
and no reader is started.

`/readyz` verifies that the shadow-indexer schema is usable by the runtime role
and that a configured reader is still running; it reports not ready if either
fails, so Kubernetes restarts the pod. With no Postgres configured it always
reports ready.

## Security Model

The health endpoints are unauthenticated internal endpoints. Restrict them to a
private network; never expose them publicly. The wildcard bind supports
container networking.

## Configuration

| Env var                                   | Default | Description                                    |
| ----------------------------------------- | ------- | ---------------------------------------------- |
| `SHADOW_METRICS_POSTGRES_URL`             | (unset) | Postgres URL. When unset, DB is disabled.      |
| `SHADOW_METRICS_POSTGRES_MAX_CONNECTIONS` | `10`    | Max Postgres pool connections.                 |
| `SHADOW_METRICS_HTTP_PORT`                | `9101`  | Health server port.                            |
| `SHADOW_METRICS_POLL_INTERVAL_SECS`       | `2`     | Seconds between polls.                         |
| `SHADOW_METRICS_MAX_ROWS_PER_POLL`        | `1000`  | Max shadow block rows fetched by one poll.     |
| `SHADOW_METRICS_METRICS_PORT`             | `9003`  | Prometheus metrics port.                       |

The `9101` HTTP default applies only when nothing overrides it. The deployment
chart sets `SHADOW_METRICS_HTTP_PORT` to `8080`, so probes in a running pod hit
`8080` and not `9101`.

`SHADOW_METRICS_POLL_INTERVAL_SECS` defaults to `2` because the writer flushes
every second and reconciliation lands in bursts roughly every ten seconds.
`SHADOW_METRICS_MAX_ROWS_PER_POLL` bounds catch-up so a long outage drains
steadily instead of loading every JSONB payload at once.

Prometheus metrics are only served when `--metrics.enabled` is passed.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
