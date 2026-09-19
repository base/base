# Acceptance testing

The acceptance runner provisions the repository's real Docker Compose devnet and
checks its externally visible RPC behavior. Scenarios are strict, versioned TOML;
each run produces a portable JSON result and an offline visual report.

The checked-in scenarios are:

- `smoke`: chain identity, block production, and validator convergence.
- `derivation`: unsafe production, safe-head derivation, and head freshness.
- `denim-transition`: progress and convergence immediately before and after Denim.

## Requirements

- Linux, Git, Rust/Cargo, Docker with Compose 2.24.4+ and Buildx, and enough resources to
  build and run the canonical devnet.
- Acceptance publishes only execution RPCs on dynamically allocated localhost ports.
- No canonical devnet containers may already exist.

Run commands from the repository root. `cargo run` compiles the CLI as needed:

```console
cargo run -p base-acceptance-cli -- validate acceptance/scenarios/smoke.toml
cargo run -p base-acceptance-cli -- plan acceptance/scenarios/smoke.toml
cargo run -p base-acceptance-cli -- run acceptance/scenarios/smoke.toml \
  --output target/acceptance/smoke
```

The output directory must be new or empty. A managed run builds images, generates
fresh chain state, verifies genesis and the rollup schedule, waits for the required
RPCs to advance, executes checks, collects bounded logs, and removes owned Docker
resources and private chain state.

## Scenario format: provision, then check

A scenario has two phases in one TOML document. Top-level, `devnet`, and
`readiness` fields describe what to provision and when it is usable; `[[checks]]`
tables describe read-only observations made after readiness.

```toml
schema_version = 1
id = "example"
description = "Production and agreement across Denim"
timeout = "10m"

# Phase 1: provisioning and readiness.
[devnet]
topology = "single-sequencer"

[devnet.l1]
chain_id = 1337
slot_duration = "12s"

[devnet.l2]
chain_id = 84538453
verifier_l1_confirmations = 15

[devnet.l2.forks]
azul = { at_block = 20 }
beryl = { at_block = 21 }
cobalt = { at_block = 22 }
denim = { at_block = 150 }
zenith = { disabled = true }

[readiness]
timeout = "4m"
request_timeout = "2s"
poll_interval = "1s"

# Phase 2: ordered, bounded checks.
[[checks]]
id = "before-denim"
kind = "head_progress"
endpoint = "builder"
minimum_blocks = 3
timeout = "30s"
start = { before_fork = "denim", chain = "l2" }

[[checks]]
id = "after-denim-agreement"
kind = "heads_converge"
endpoints = ["builder", "validator"]
head = "latest"
max_lag_blocks = 5
timeout = "90s"
start = { after_fork = "denim", chain = "l2" }
```

Unknown fields are rejected. Defaults are the single-sequencer topology, chain
IDs shown above, a 12-second L1 slot, 15 confirmations, and L2 forks Azul/Beryl/
Cobalt/Denim at blocks 20/21/22/25 with Zenith disabled. Supported endpoint roles
are `l1`, `builder`, `validator`, `rpc`, and `shadow`. Supported check kinds are:

- `chain_id`: `endpoint`, `expected`, and `timeout`.
- `head_progress` and `safe_head_progress`: `endpoint`, `minimum_blocks`, and `timeout`.
- `heads_converge`: at least two `endpoints`, optional `head` (`latest`, `safe`, or
  `finalized`), `max_lag_blocks`, and `timeout`.
- `head_fresh`: `endpoint`, `maximum_age`, `duration`, and `timeout`.

Every check may have one L2 `start` condition with exactly one of `before_fork` or
`after_fork`. Checks run in file order. See `scenarios/` for complete examples.

## CLI workflows

Validation and planning do not contact Docker or RPCs. `plan` prints the resolved
configuration, including defaults, as JSON:

```console
cargo run -p base-acceptance-cli -- validate acceptance/scenarios/*.toml
cargo run -p base-acceptance-cli -- plan acceptance/scenarios/denim-transition.toml
```

Use `--no-build` only when the required images have already been built and loaded:

```console
cargo run -p base-acceptance-cli -- run acceptance/scenarios/smoke.toml \
  --output target/acceptance/smoke-cached --no-build \
  --run-id local-smoke --tested-sha "$(git rev-parse HEAD)"
```

Attach mode performs no provisioning, log collection, or cleanup. The endpoint
file is a JSON object mapping logical roles to credential-free HTTP(S) URLs. Add
`--rollup` when a scenario has a fork-relative check (or when schedule reporting
is desired); its chain IDs and fork timestamps are verified before use.

```json
{"l1":"http://127.0.0.1:4545","builder":"http://127.0.0.1:7545","validator":"http://127.0.0.1:8545"}
```

```console
cargo run -p base-acceptance-cli -- check acceptance/scenarios/smoke.toml \
  --endpoints endpoints.json --output target/acceptance/attached
```

Re-render a portable result and its referenced evidence into another directory:

```console
cargo run -p base-acceptance-cli -- report \
  target/acceptance/smoke/report/result.json --output target/acceptance/rendered
```

Sharded CI first records exactly which scenarios and checks are expected, then
strictly aggregates one matching result for each scenario:

```console
cargo run -p base-acceptance-cli -- manifest acceptance/scenarios/*.toml \
  --run-id run-1 --tested-sha "$GIT_SHA" --output expected.json
cargo run -p base-acceptance-cli -- aggregate --expected expected.json \
  --results downloaded-results --output target/acceptance/aggregate
```

The CI-only `publish` command validates aggregate artifacts and safely updates the
marker-owned PR comment. Its `GitHub` credentials and provenance inputs are supplied
by the trusted-base publication job; it is not part of the local runner workflow.
It accepts `--event`, `--result`, `--expected`, `--run-id`, `--attempt`, and
`--started-at`, with `GITHUB_TOKEN`, `GITHUB_REPOSITORY`, `TESTED_SHA`, and optional
`BOT_LOGIN` in the environment.

If a managed process is killed before cleanup, recover only the resources named
by its private ownership manifest:

```console
cargo run -p base-acceptance-cli -- cleanup \
  --manifest target/acceptance/smoke/private/ownership.json
```

## Results and status

A run writes these user-facing files below `<output>/report/`:

- `report.html`: self-contained offline visual report; open it in a browser.
- `result.json`: versioned portable source of truth.
- `summary.md`: concise CI/PR summary.
- `evidence/heads.json` and, for managed runs, `evidence/compose.log`.
- `environment.json` for managed runs, containing inspected image IDs and whether
  this invocation built them.

Check and lifecycle statuses are `passed`, `failed`, `error`, `blocked`, and
`cancelled`. Exit code 0 means all checks passed, 1 means an assertion failed, 2
means invalid invocation/configuration, and 3 means infrastructure prevented a
complete evaluation. Infrastructure takes precedence when outcomes are mixed.

The advisory Depot workflow validates scenarios, runs one selected shard, uploads
its report bundle, and strictly aggregates expected results. Reports are artifacts;
PR publication is available only when the trusted base revision contains the
publisher. Same-repository PRs run smoke automatically; manual dispatch selects
any checked-in scenario. The publisher updates only its marker-owned
`depot-code-access[bot]` comment (override with the `BOT_LOGIN` repository variable
for a different bot). Reports remain artifact-only on the first introduction PR.

## Testing the runner and report

```console
cargo test --locked -p base-acceptance -p base-acceptance-cli
cargo run -p base-acceptance-cli -- report \
  acceptance/fixtures/reports/synthetic-mixed.json --output target/acceptance/preview
```

The synthetic fixture deliberately contains a failure and a startup error, so
the final command returns exit code 3 while still writing the visual report.
It is a rendering example, not evidence of a real devnet run.

## Limitations and safety

- Only the fixed `single-sequencer` topology is supported. Fixed container names
  and its subnet, plus a host lock, mean acceptance runs must be serial and cannot
  share a machine with the canonical developer devnet.
- Configurable L1 forks, including Glamsterdam, are unsupported: the pinned
  devnet generator does not expose a compatible Glamsterdam schedule.
- `--no-build` inspects and reports cached local image IDs. It does **not** prove
  that those images correspond to the current source revision.
- Attach mode validates RPC identity and behavior but does not own the target or
  apply the provisioning configuration. A supplied rollup verifies its schedule.
- Results are bounded observations, not a proof of protocol correctness. A fork
  crossing records the first observed active block, not proof of fork behavior.
