# E2E Tests

This directory hosts the end-to-end tests for `op-succinct`, built on top of
Optimism's devstack. Use the `justfile` here to prepare contract artifacts and
run the suite with the expected environment.

## Layout

- `e2e/`: Go e2e tests.
  - `nodes/`: Nodes-only tests (no proposer) for local development.
  - `validity/`: Validity proposer tests.
  - `faultproof/`: Fault proof proposer tests.
- `artifacts/`: Contract artifacts; a compressed tarball lives at
  `artifacts/compressed/artifacts.tzst` and is unpacked into `artifacts/src`
  before tests.
- `optimism/`: Vendored `succinctlabs/optimism` repository using
  `op-succinct-sysgo` branch, which contains `op-succinct` integration code with
  Optimism devstack.
- `bindings/`, `presets/`, `utils/`: Presets, helpers and generated code used by
  the tests.
- `monitoring/`: Grafana dashboards and Prometheus datasource configs for metrics.

## Prerequisites

- Go 1.23+ (matches `go.mod`).
- Rust toolchain (to build the `validity` binary).
- `just`, `zstd`, and `tar` available on your PATH.

## Setup

1. From this directory, fetch/update contract artifacts:

   ```just
   just update-packages
   ```

   This rebuilds artifacts via the vendored Optimism deployer and saves them to
   `artifacts/compressed/artifacts.tzst`.

2. Unpack artifacts (auto-run by the test target, but available standalone):

   ```just
   just unzip-contract-artifacts
   ```

## Running the e2e Suite

- Run everything (builds both binaries, unpacks artifacts):

  ```just
  just test-e2e-sysgo
  ```

- Validity proposer only:

  ```just
  just test-e2e-sysgo ./e2e/validity/...
  ```

- Faultproof proposer only:

  ```just
  just test-e2e-sysgo ./e2e/faultproof/...
  ```

- Run a single test with a filter:

  ```just
  just test-e2e-sysgo ./e2e/validity/... TestValidityProposer_ProveSingleRange
  ```

## Long-Running Tests

Keep the stack running indefinitely for manual debugging. Press `Ctrl+C` to stop.
Output is logged to `tests/logs/<mode>-<timestamp>.log`.

### Nodes Only

Start L1/L2 nodes without a proposer for local development:

```bash
just long-running nodes
```

This creates the following files:

| File | Purpose |
|------|---------|
| `tests/.env` | RPC endpoints using `127.0.0.1` for running the proposer natively |
| `tests/.env.docker` | RPC endpoints using `host.docker.internal` for Docker containers |
| `configs/L1/900.json` | L1 chain config (chain ID 900 for local devnet) |

To run a proposer in Docker against the local devnet:

```bash
docker compose --env-file tests/.env.docker run \
  -v ./configs/L1/900.json:/app/configs/L1/900.json \
  op-succinct
```

### With Proposer

Run nodes with a proposer:

```bash
just long-running validity      # Validity proposer
just long-running faultproof    # Fault proof proposer
just long-running faultproof-ff # Fault proof with fast finality
```

At startup, an env file is written with variables needed for debugging:

| Mode | Env File |
|------|----------|
| validity | `.env.validity` |
| faultproof / faultproof-ff | `.env.faultproof` |

Source it to use with tools like `cast`:

```bash
source .env.validity
cast block-number --rpc-url $L2_RPC
```

## Monitoring

Metrics are disabled by default. Enable Grafana and Prometheus for debugging by
setting `SYSGO_METRICS_ENABLED=true`:

```bash
SYSGO_METRICS_ENABLED=true just long-running validity
SYSGO_METRICS_ENABLED=true just long-running faultproof
SYSGO_METRICS_ENABLED=true just long-running faultproof-ff
```

> **Note**: Run only one test at a time when metrics are enabled. Multiple
> concurrent tests will cause port conflicts.

| Service    | URL                   | Credentials |
|------------|-----------------------|-------------|
| Grafana    | http://localhost:3000 | admin/admin |
| Prometheus | http://localhost:9999 | -           |

Dashboard configurations are located in `monitoring/grafana/dashboards/`.

## Maintenance

- `tests/optimism` is a git submodule that pins the
  `succinctlabs/optimism` fork on the `op-succinct-sysgo` branch, which carries
  Succinct-specific devstack changes. Rebase that branch onto
  `ethereum-optimism/optimism` (`develop` branch) whenever we need upstream fixes,
  new OP Stack features, or contract updates that affect the e2e suite.
- Changes to the fork should land via PRs into `succinctlabs/optimism` targeting
  `op-succinct-sysgo`.
- After merging fork updates, advance the `tests/optimism` submodule to the new
  commit, run `go mod tidy` in `tests` if dependencies changed, regenerate
  artifacts with `just update-packages`, and rerun tests to confirm the vendored
  stack still passes our tests.
