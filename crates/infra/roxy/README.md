# `roxy`

JSON-RPC reverse proxy for Base infrastructure. Replaces the `ProxyD` fork with a
Rust service we own, focused on the features we actually run in production.

> **Status:** scaffolding only. No proxying yet.

## Usage

```bash
roxy \
  --listen-addr 0.0.0.0:8545 \
  --backend rpcs=http://127.0.0.1:8545,http://127.0.0.1:8546 \
  --backend flashblocks=http://127.0.0.1:9545
```

Backend flags use `name=url[,url...]` and may be repeated. The equivalent
environment variable uses semicolons between backends:

```bash
ROXY_BACKENDS='rpcs=http://127.0.0.1:8545,http://127.0.0.1:8546;flashblocks=http://127.0.0.1:9545'
```

Literal commas and semicolons in URLs must be percent-encoded.

`GET /healthz` is the liveness probe. `GET /readyz` is the readiness probe.

Run `roxy --help` for the full flag list. Every flag also accepts an env var
(see `--help`).
