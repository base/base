# `roxy`

JSON-RPC reverse proxy for Base infrastructure. Replaces the ProxyD fork with a
Rust service we own, focused on the features we actually run in production.

> **Status:** scaffolding only. No proxying yet.

## Goals

- Own the proxy path end-to-end (no ProxyD fork)
- Ship only production-used features (whitelist, cache, consistent hash, batch,
  rate limits, health probes, compliance checks, tracing)
- CLI flags + env vars (no TOML config in v1)
- Integrate with Docker / codified devnets and the e2e test framework

## Non-goals (v1)

- Consensus-aware routing
- Automatic request retries (caller responsibility)
- Shared remote config service (later)

## Usage

```bash
roxy --listen-addr 0.0.0.0:8545
```

`GET /healthz` is the liveness probe. `GET /readyz` is the readiness probe.

Run `roxy --help` for the full flag list. Every flag also accepts an env var
(see `--help`).
