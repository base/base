# `based`

Small **sidecar** that watches an execution node’s JSON-RPC HTTP endpoint, classifies whether the chain head looks **healthy / delayed / unhealthy / errored**, and **heartbeats those states to StatsD** (Datadog agent–compatible UDP).

- **Package:** `based-bin` → binary **`based`** ([`Cargo.toml`](./Cargo.toml)).
- **Logic:** [`based`](../../crates/infra/based) library (`BlockProductionHealthChecker`, `AlloyEthClient`, `HealthcheckMetrics`).

## Quick start

```sh
cargo run -p based-bin -- --help
```

Point `--node-url` (or env) at your node’s HTTP RPC. For metrics, run a StatsD listener on **`8125`** (see [StatsD](#statsd)) or set `DD_AGENT_HOST` to your agent.

## What it does (runtime)

1. Init tracing via `define_log_args!("BBHC_SIDECAR")` ([`src/main.rs`](./src/main.rs)).
2. Open a **UDP StatsD** sink → `{DD_AGENT_HOST or 127.0.0.1}:8125`, prefix **`base.blocks`**, tags from `CODEFLOW_*` when set.
3. Build `AlloyEthClient` + `BlockProductionHealthChecker`, then:
   - **`poll_for_health_checks()`** — periodic RPC polls (see [Polling & health model](#polling--health-model)).
   - **`spawn_status_emitter(2000)`** — StatsD counter increments on a **2 s** cadence (decoupled from poll rate).

Shutdown: **Ctrl+C** (poll loop is selected against a Ctrl+C handler in `main`).

## Polling & health model

- **RPC:** each cycle calls Alloy `get_block(latest)` with **transaction hashes only** (`.hashes()` in [`alloy_client.rs`](../../crates/infra/based/src/healthcheck/alloy_client.rs)); each fetch has a **2 s** timeout ([`run_health_check`](../../crates/infra/based/src/healthcheck/mod.rs)).
- **Timing:** uses Tokio `interval` — the **first** poll runs **immediately**, then about every `BBHC_SIDECAR_POLL_INTERVAL_MS` (default **1000**).
- **Classification** (after a **successful** fetch, wall clock vs block timestamp, second granularity):

  | State | Condition |
  |-------|-----------|
  | Healthy | block age ≤ `grace_period_ms` |
  | Delayed | age **>** grace **and** age **<** `unhealthy_node_threshold_ms` |
  | Unhealthy | age **≥** `unhealthy_node_threshold_ms` |

- **`--new-instance` / `NEW_INSTANCE` (default `true`):** while starting up, a **successful** poll is treated as **healthy** and delayed/unhealthy **logs** are suppressed until the head is fresh enough (then the flag clears). **RPC failures and timeouts still flip to the error state** — only log severity is softened (`debug`).

## StatsD

| Item | Value |
|------|--------|
| Host / port | `DD_AGENT_HOST` or `127.0.0.1`, port **`8125`** (fixed in `main.rs`) |
| Prefix | `base.blocks` → emitted counters like **`base.blocks.healthy`**, **`base.blocks.delayed`**, … (cadence + `incr("…")`) |
| Heartbeat | Same 2 s **interval** task as above — **first tick is immediate**, then every 2 s |
| Tags | `configname`, `environment`, `projectname`, `servicename` from `CODEFLOW_CONFIG_NAME`, `CODEFLOW_ENVIRONMENT`, `CODEFLOW_PROJECT_NAME`, `CODEFLOW_SERVICE_NAME` (`unknown` if unset). Cadence emits DogStatsD-style `|#…` tag suffixes. |

## Repo devnet / Docker

[`etc/docker/README.md`](../../etc/docker/README.md)’s `Dockerfile.rust-services` target list **does not** include **`based`**; compose under `etc/docker` has **no** `BBHC_*` wiring for this binary. The root [`Justfile`](../../Justfile) loads `etc/docker` devnet recipes and **does not** start `based`. Treat this as an **operator-run** sidecar, not part of the default local devnet stack.

## Environment variables

| Variable | Role |
|----------|------|
| `NODE_URL` | If set in the process environment, **this wins** over the parsed `--node-url` value when building the RPC client (see [Note on `NODE_URL`](#note-on-node_url)). |
| `BBHC_SIDECAR_POLL_INTERVAL_MS` | Poll period (ms), default `1000`. |
| `BBHC_SIDECAR_GRACE_PERIOD_MS` | Grace window (ms), default `2000`. |
| `BBHC_SIDECAR_UNHEALTHY_NODE_THRESHOLD_MS` | Unhealthy threshold (ms), default `3000`. |
| `NEW_INSTANCE` | Maps to `--new-instance` (default `true`); behavior in [Polling & health model](#polling--health-model). |
| `DD_AGENT_HOST` | StatsD host. |
| `CODEFLOW_CONFIG_NAME` / `CODEFLOW_ENVIRONMENT` / `CODEFLOW_PROJECT_NAME` / `CODEFLOW_SERVICE_NAME` | StatsD tags. |
| `BBHC_SIDECAR_LOG_VERBOSITY` | `1`–`5` (default `3` = INFO). |
| `BBHC_SIDECAR_LOG_FORMAT` | Stdout format. |
| `BBHC_SIDECAR_LOG_DIR` | File log directory (optional). |

Sourced from `bin/based/src/main.rs` and `define_log_args!("BBHC_SIDECAR")` in [`macros.rs`](../../crates/utilities/cli/src/macros.rs). Other log flags (`--quiet`, file format, rotation, …) exist on the CLI only — use `--help` for the full set.

### Note on `NODE_URL`

Clap also binds the `node_url` field to **`NODE_URL`** for defaults. Independently, `main` calls **`std::env::var("NODE_URL")`**: if that variable is present, it **overrides** `args.node_url` even when the latter came from **`--node-url`** (clap normally prefers CLI over env for the field, but the second read forces env to win). Deployment comments in `main.rs` mention mapping something like **`BBHC_SIDECAR_GETH_RPC`** from a ConfigMap into **`NODE_URL`**; **this binary never reads `BBHC_SIDECAR_GETH_RPC` itself.**
