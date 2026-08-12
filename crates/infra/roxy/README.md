# `roxy`

JSON-RPC reverse proxy for Base infrastructure. Replaces the `ProxyD` fork with a
Rust service we own, focused on the features we actually run in production.

> **Status:** scaffolding only. No proxying yet.

## Usage

```bash
roxy --listen-addr 0.0.0.0:8545
```

`GET /healthz` is the liveness probe. `GET /readyz` is the readiness probe.

Run `roxy --help` for the full flag list. Every flag also accepts an env var
(see `--help`).
