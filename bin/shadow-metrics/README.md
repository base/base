# `shadow-metrics`

Service process for the shadow block explorer API. It connects to Postgres for
shadow candidate blocks that were reorged out, serves them over a read-only HTTP
JSON API, and exposes Kubernetes-style health probes.

## Behavior

On startup, the service connects to Postgres when a Postgres host is configured,
then starts an HTTP server exposing the block API and health probes (`GET
/healthz`, `GET /readyz`).

When no Postgres host is configured, database connectivity is disabled: the
block API responds `503` and `/readyz` always reports ready.

`/readyz` verifies that the shadow-indexer schema is readable by the runtime
role; it reports not ready if that check fails, so Kubernetes restarts the pod.
With no Postgres configured it always reports ready.

## Security Model

The health endpoints are unauthenticated internal endpoints. Restrict them to a
private network; never expose them publicly. The wildcard bind supports
container networking.

## Configuration

| Env var                                   | Default          | Description                                |
| ----------------------------------------- | ---------------- | ------------------------------------------ |
| `SHADOW_METRICS_POSTGRES_HOST`            | (unset)          | Postgres host. When unset, DB is disabled. |
| `SHADOW_METRICS_POSTGRES_PASSWORD`        | (unset)          | Password for the Postgres role.            |
| `SHADOW_METRICS_POSTGRES_PORT`            | `5432`           | Postgres port.                             |
| `SHADOW_METRICS_POSTGRES_DATABASE`        | `shadow_metrics` | Postgres database name.                    |
| `SHADOW_METRICS_POSTGRES_USER`            | `app`            | Postgres role to authenticate as.          |
| `SHADOW_METRICS_POSTGRES_MAX_CONNECTIONS` | `10`             | Max Postgres pool connections.             |
| `SHADOW_METRICS_HTTP_PORT`                | `9101`           | Health + block API server port.            |
| `SHADOW_METRICS_METRICS_PORT`             | `9003`           | Prometheus metrics port.                   |

Setting `SHADOW_METRICS_POSTGRES_HOST` requires `SHADOW_METRICS_POSTGRES_PASSWORD`.

The `9101` HTTP default applies only when nothing overrides it. The deployment
chart sets `SHADOW_METRICS_HTTP_PORT` to `8080`, so probes in a running pod hit
`8080` and not `9101`.

Prometheus metrics are only served when `--metrics.enabled` is passed.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
