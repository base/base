# `shadow-metrics`

Service process for shadow metrics. It connects to Postgres from config and
exposes Kubernetes-style health probes.

## Behavior

On startup, the service connects to Postgres when
`SHADOW_METRICS_POSTGRES_URL` is set, starts an HTTP health server (`GET
/healthz`, `GET /readyz`), and runs an idle heartbeat loop. `/readyz` verifies
the shadow-indexer schema is usable by the runtime role when a connection is
configured; otherwise it always reports ready.

## Security Model

The health endpoints are unauthenticated internal endpoints. Restrict them to a
private network; never expose them publicly. The wildcard bind supports
container networking.

## Configuration

| Env var                              | Default | Description                                  |
| ------------------------------------ | ------- | -------------------------------------------- |
| `SHADOW_METRICS_POSTGRES_URL`        | (unset) | Postgres URL. When unset, DB is disabled.    |
| `SHADOW_METRICS_POSTGRES_MAX_CONNECTIONS` | `10` | Max Postgres pool connections.               |
| `SHADOW_METRICS_HTTP_PORT`           | `9101`  | Health server port.                          |
| `SHADOW_METRICS_HEARTBEAT_INTERVAL_SECS` | `30` | Idle heartbeat log interval in seconds.      |
| `SHADOW_METRICS_METRICS_PORT`        | `9003`  | Prometheus metrics port.                     |

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
