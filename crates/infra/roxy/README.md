# `roxy`

JSON-RPC reverse proxy for Base infrastructure. Replaces the `ProxyD` fork with a
Rust service we own, focused on the features we actually run in production.

> **Status:** single-backend JSON-RPC passthrough. Batch, whitelist, and
> multi-backend routing come in follow-up PRs.

## Usage

```bash
roxy \
  --listen-addr 0.0.0.0:8545 \
  --backend rpcs=http://127.0.0.1:8545,http://127.0.0.1:8546
```

Exactly one `--backend` is required. Extra URLs on that backend are accepted and
stored; traffic currently uses the first URL.

`POST /` accepts a single JSON-RPC request object and forwards it to the
backend. Batch arrays are rejected. `GET /healthz` is liveness; `GET /readyz`
is readiness.

Run `roxy --help` for the full flag list. Every flag also accepts an env var
(see `--help`).
