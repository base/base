# Configurable devnet acceptance testing

Status: design reference from September 19, 2026. The initial runner, scenarios,
reports, and advisory workflow are now implemented; see [README.md](README.md)
for the supported interface. The implementation publishes RPCs on dynamic
localhost ports instead of requiring the canonical host ports. L1 generator/client
fork extensions and HA remain follow-up work, as described below.

Quick navigation: [layout](#3-repository-layout-and-ownership),
[TOML](#4-scenario-interface), [provisioning](#5-provisioning-and-isolation),
[fork semantics](#6-checks-time-and-activation-evidence),
[visual reports](#8-visual-actionable-reporting),
[Depot](#9-depot-workflow-and-pr-publication),
[milestones](#10-delivery-milestones-and-implementation-file-targets),
[verification](#11-verification-strategy).

## 1. Outcome and decisions

An acceptance scenario must answer two questions:

1. **What system should run?** Compose topology, chain configuration, supported
   fork schedule, and node settings.
2. **What must that system do?** Named, typed checks with explicit observation
   windows and actionable failure evidence.

Keep those responsibilities separate internally, but describe them in one TOML
file and execute them through one command. Put the library, scenarios, report
assets, fixtures, and documentation in this top-level `acceptance/` directory.
Keep the binary in `bin/acceptance/`, as required by repository guidance.

Decisions:

- Reuse `etc/docker` Compose and genesis setup; do not copy the deployment into a
  parallel acceptance-only Compose stack or replace it with simulated nodes.
- Start with the current single-sequencer topology. It still contains L1,
  batching, validators, and observability; it is not a lightweight two-process net.
- Run one provisioned scenario per Docker host at a time initially. Depot can
  parallelize scenarios across isolated runners, not within a shared Docker daemon.
- Use a finite set of Rust check variants, not arbitrary shell, RPC-expression,
  JSONPath, or template evaluation in TOML.
- Produce `result.json`, self-contained `report.html`, and `summary.md` from one
  result model. No frontend build system, hosted dashboard, or external assets.
- Preserve a read-only attach mode for an already-running devnet.
- Begin CI as advisory but retain truthful failure conclusions. Do not turn
  assertion failures or absent results green to make the rollout non-blocking.
- Treat configurable L1 forks as an explicit generator/client workstream.
  **Glamsterdam is not supported merely by adding a TOML field.**

Not initial scope: fault injection, HA failover, snapshot restoration, arbitrary
Compose-file injection, a generic test language, public report hosting, or
performance benchmarking. HA and typed transaction actions are later increments.

## 2. Repository evidence and compatibility constraints

| Existing component | Consequence for implementation |
| --- | --- |
| [Devnet recipes](../etc/docker/Justfile), particularly `up-single`, `_up-devnet`, and `down` | Existing startup deletes `.devnet` and invokes global teardown. The acceptance runner must not call it unchanged. |
| [Base Compose](../etc/docker/docker-compose.yml) and [HA overlay](../etc/docker/docker-compose.ha.yml) | Reusable real deployment, but fixed names, ports, subnet, and bind mounts prevent shared-host parallel runs. |
| [Devnet environment](../etc/docker/devnet-env) | L1/L2 IDs are 1337/84538453; verifier L1 confirmations are actually 15, regardless of the adjacent default comment. |
| [Setup image](../etc/docker/Dockerfile.devnet) | Builds a pinned external `base/optimism` generator and beacon-genesis tooling. Setup is an upstream interface, not local Rust logic. |
| [System-test setup](../etc/systems/src/setup/container.rs) | Already consumes the same generator. Any generator extension must preserve this consumer. |
| [Upgrade-signal tooling](../etc/scripts/devnet/upgrade-signal.sh) | Distinguishes generated schedule from runtime contract state; can source the default devnet environment again. Do not let it silently override a resolved scenario. |
| [Depot system tests](../.depot/workflows/ci-core.yml) | Real Docker-backed testing already runs on 16-core Depot runners. |
| [PR CI](../.depot/workflows/ci-pr.yml) and [merge queue](../.depot/workflows/ci-merge-queue.yml) | Both currently set `run_system_tests: true`; do not infer otherwise from stale testing prose. |
| [IAI workflow](../.depot/workflows/bench-iai.yml), [renderer](../etc/scripts/ci/iai_compare.py), and [comment helper](../etc/scripts/ci/post_pr_comment.sh) | Reuse the concise/collapsible presentation pattern. The helper currently hardcodes the IAI marker and does not paginate. |
| [Workspace manifest](../Cargo.toml) and [Docker Rust build](../etc/docker/Dockerfile.rust-services) | Explicitly add the new library member and copy its build inputs into `workspace-source`, or Cargo Chef/workspace resolution breaks. |
| [Affected-crate discovery](../etc/scripts/local/affected-crates.py) | Uses Cargo metadata, so the new package should be discoverable without special path logic. Verify this rather than adding another hardcoded list. |

### Verified generator interface

Research inspected the generator revision pinned by `Dockerfile.devnet`:
[source tree](https://github.com/base/optimism/tree/0066b17c3fe0cbb5ea935de6d5b18d4fc86dc439/op-deployer/cmd/op-deployer).

- [Arguments](https://github.com/base/optimism/blob/0066b17c3fe0cbb5ea935de6d5b18d4fc86dc439/op-deployer/cmd/op-deployer/args.go)
  support chain IDs, slot duration, activation admin, and L2 Isthmus, Azul, Beryl,
  Cobalt, Denim, and Zenith block offsets. Compose currently wires only a subset.
- [L1 execution template](https://github.com/base/optimism/blob/0066b17c3fe0cbb5ea935de6d5b18d4fc86dc439/op-deployer/cmd/op-deployer/assets/l1-el-genesis.json.template)
  fixes existing fork times through Osaka/BPO2 at genesis; only chain ID and
  timestamp are substituted. It has no arbitrary L1 fork scheduling input.
- [Beacon template](https://github.com/base/optimism/blob/0066b17c3fe0cbb5ea935de6d5b18d4fc86dc439/op-deployer/cmd/op-deployer/assets/l1-cl-config.yaml.template)
  fixes fork epochs through Fulu at zero, fork versions, and `GENESIS_DELAY: 0`.
  Slot duration and genesis timestamp are configurable, epoch geometry is not.
- [Generation](https://github.com/base/optimism/blob/0066b17c3fe0cbb5ea935de6d5b18d4fc86dc439/op-deployer/cmd/op-deployer/main.go)
  accepts `BASE_DEVNET_TIMESTAMP`; otherwise it captures current Unix time during
  setup. There is no built-in relative genesis delay. Startup can consume a
  substantial part of a near-genesis fork window.
- [L2 scheduling](https://github.com/base/optimism/blob/0066b17c3fe0cbb5ea935de6d5b18d4fc86dc439/op-deployer/cmd/op-deployer/upgrades.go)
  converts block offsets into timestamps. After Denim it assumes five blocks per
  second and requires post-Denim offsets to be divisible by five relative to Denim.
  Azul also sets L2 `osakaTime`. Do not reinterpret these inputs as native
  block-number activation fields.
- Outputs include L1 `el/genesis.json`, `el/chain-config.json`, `cl/config.yaml`,
  `cl/genesis.ssz`, and L2 `genesis.json`, `rollup.json`, `rollup-conductor.json`,
  `l1-addresses.json`, and possibly `upgrade-signal.env`. Completion markers can
  cause existing output to be reused: provisioned acceptance runs need fresh roots.

Glamsterdam support was not found in this pinned interface. Its compatible EL,
CL, and beacon-genesis tool versions remain a research/implementation dependency.

## 3. Repository layout and ownership

```text
acceptance/
  Cargo.toml                    # base-acceptance library
  README.md
  IMPLEMENTATION_PLAN.md
  scenarios/
    smoke.toml
    denim-transition.toml
    derivation.toml
  src/
    lib.rs                      # module declarations and re-exports only
    config.rs                   # TOML types, validation, resolved config
    provision.rs                # owned Compose lifecycle and manifests
    runner.rs                   # readiness, observation windows, coordination
    check.rs                    # typed checks and RPC observation boundary
    result.rs                   # versioned results and aggregate policy
    report.rs                   # HTML, Markdown, fixed SVG rendering
    cli.rs                      # CLI definition/dispatch logic
  report/
    style.css                   # embedded, no network assets
  fixtures/
    reports/                    # synthetic passed/failed/error/partial cases
    config/                     # valid and invalid config fixtures
  Justfile                      # validate/run/report entrypoints
bin/acceptance/
  Cargo.toml                    # base-acceptance-cli; binary base-acceptance
  README.md
  src/main.rs                   # minimal runtime/logging/dispatch glue
```

This is an initial responsibility map, not a requirement to create empty modules.
Split a file only when its responsibilities actually grow. Do not introduce a
second reporting crate, plugin system, generic scheduler, or redundant wrappers.

Use existing workspace Serde, TOML, Tokio, Clap, Alloy, URL, and duration tooling.
Avoid importing the entire system-test or load-test stack merely for RPC polling.
Follow root guidance for public/re-exported types, documentation, workspace lints,
dependency formatting, and colocated unit tests.

Add `acceptance` to workspace members; existing `bin/[a-o]*` discovers the CLI.
Add the library path dependency in the workspace. Keep ordinary unit tests in
normal workspace CI. Real-devnet tests must be explicitly invoked/ignored so
`cargo test --workspace` does not unexpectedly launch Docker.

Generated state lives outside tracked source:

```text
target/acceptance/<run-id>/<scenario-id>/
  private/                      # restrictive permissions, never uploaded
    compose.env
    compose.resolved.yaml
    devnet/                     # genesis, keys, node databases
    ownership.json
  report/                       # sanitized, portable upload boundary
    result.json
    report.html
    summary.md
    scenario.resolved.toml
    environment.json
    evidence/
```

`target/` is already ignored and excluded from Docker build contexts. Do not add
redundant ignore rules. Do not upload raw Compose configuration, JWTs, private
keys, validator directories, or database files.

## 4. Scenario interface

The following is the proposed version-one shape. Timing values are initial
examples to measure, not proven CI budgets.

```toml
schema_version = 1
id = "denim-transition"
description = "Production and validator catch-up across Denim"
timeout = "10m"

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

[[checks]]
id = "chain-identity"
kind = "chain_id"
endpoint = "builder"
expected = 84538453
timeout = "10s"

[[checks]]
id = "production-before-denim"
kind = "head_progress"
endpoint = "builder"
minimum_blocks = 3
timeout = "30s"
start = { before_fork = "denim", chain = "l2" }

[[checks]]
id = "production-after-denim"
kind = "head_progress"
endpoint = "builder"
minimum_blocks = 10
timeout = "30s"
start = { after_fork = "denim", chain = "l2" }

[[checks]]
id = "validator-catches-up"
kind = "heads_converge"
endpoints = ["builder", "validator"]
head = "latest"
max_lag_blocks = 5
timeout = "60s"
start = { after_fork = "denim", chain = "l2" }
```

### Configuration rules

- `schema_version`, scenario ID, and nonempty checks are mandatory. Stable IDs
  identify report anchors and CI matrix entries; reject duplicates after
  normalization, unknown keys, unknown variants, and unsupported versions.
- Defaults come from the checked-in devnet configuration at the tested revision.
  Emit the entire resolved configuration, not just scenario overrides. Omission
  inherits a documented default; explicit `disabled` means disabled and must not
  fall through to `devnet-env` defaults. Reject unsupported disabling/dependencies.
- Typed endpoint names (`l1`, `builder`, `validator`, `rpc`, plus explicit CL
  endpoints for sync status) resolve from the launched deployment manifest.
  Scenario authors do not embed local ports or Docker IP addresses in checks.
- Omitted `start` means after readiness. A before-fork window starts after
  readiness and must finish before the verified boundary; an after-fork window
  starts when readiness and boundary observation both hold. Its `timeout` starts
  then; waiting for activation is bounded by the scenario timeout. Check kinds
  own their temporal semantics: progress/convergence are eventually checks,
  while freshness requires an explicit observation `duration`.
- Validate positive bounded durations, request/poll/window relationships,
  prerequisite fork ordering, capability availability, required services, and
  whether check windows can fit before startup. Revalidate against actual output.
- No arbitrary shell actions, raw Compose overrides, environment interpolation,
  secret literals, or custom image commands in scenario TOML. Advanced topology
  support must be a reviewed extension, not an escape hatch bypassing validation.
- Build/image selection belongs to the invocation and manifest. CI enforces the
  tested merge revision; a scenario cannot silently swap PR binaries for `main`.
- Start with self-contained files, not includes/inheritance or Cartesian-product
  syntax. CI enumerates selected files into its matrix.

### Planned commands

```sh
base-acceptance validate acceptance/scenarios/*.toml
base-acceptance plan acceptance/scenarios/denim-transition.toml
base-acceptance run acceptance/scenarios/smoke.toml --output target/acceptance/local
base-acceptance check acceptance/scenarios/smoke.toml --endpoints endpoints.json
base-acceptance report path/to/result.json --output path/to/report
base-acceptance aggregate --expected expected.json --results results/ --output report/
base-acceptance cleanup --manifest path/to/private/ownership.json
```

`validate` performs offline schema/capability checks; `plan` prints the resolved
configuration and necessary builds without modifying Docker. `run` owns fresh
provisioning and teardown. `check` attaches read-only and never builds, stops,
deletes, schedules upgrades, or submits transactions. Boundary checks in attach
mode require explicit schedule artifacts and fail when the pre-window was missed.

CLI flags override operational settings only (output, build policy, endpoints);
test expectations stay in TOML. Represent commands as argument arrays internally,
not shell-evaluated strings. The report's reproduction command is inert text.

## 5. Provisioning and isolation

Execution order:

```text
validate → resolve/build → preflight ownership → generate fresh genesis
         → verify effective configuration → start services and observers
         → readiness → check windows → collect evidence → teardown → report
```

1. Create an ownership manifest before starting resources. Include run ID, attempt,
   scenario ID, project name, Docker context, absolute data root, and tested SHA.
2. Reuse Bake's `base` and `op-batcher` targets and the setup Dockerfile. Add a
   `devnet-setup` Bake target if needed to give local and Depot builds one recipe.
   Preserve the devnet build profile/ELF settings used by current Just recipes.
3. Parameterize the `.devnet` bind-mount root in the existing Compose files while
   retaining the existing default for developer commands. Use an absolute run
   directory for acceptance. Audit every selected service and HA mount, not only
   setup outputs. Relative paths are resolved against the first Compose file.
4. Use a unique project name and ownership labels, but do not call that complete
   isolation: explicit container names, fixed host ports, and static subnet remain.
   Acquire a host-level lock scoped to the Docker daemon and fail preflight if an
   existing developer stack or unrelated conflicting resources are present.
   Never stop them. An acceptance lock cannot stop an unrelated user starting a
   stack later; conflict handling must still fail without deleting foreign state.
5. Generate an override env file from the resolved typed configuration. Scrub
   inherited scenario-sensitive variables before Compose; shell environment would
   otherwise beat `--env-file`. Preserve only required execution environment,
   Docker context, PATH, and explicitly allowed build credentials in build steps.
6. Run setup as a separate phase, validate generated EL/CL/L2 configuration, and
   only then start consumers. Confirm fork fields agree with the requested
   schedule and generated timestamps. Missing fields are errors, not defaults.
7. Inspect actual image IDs/digests and container commands. Persist sanitized
   provenance and a runtime endpoint manifest. Disable Compose build/pull during
   startup after all required images are explicitly prepared.
8. Collect bounded logs/state before shutdown; finalize cleanup outcome afterward.
   Always try to produce a report even when setup, readiness, or teardown fails.

Normal completion and SIGINT/SIGTERM need bounded cleanup of only owned resources.
Use manifest/project/labels for targeted deletion; never Docker prune or the
existing global `just devnet down`. Keep raw state private until diagnostics are
captured, then remove owned datadirs. Retention for debugging must be explicit and
must still stop containers. SIGKILL/runner loss cannot guarantee cleanup or report
delivery; isolated ephemeral CI runners and an explicit local cleanup command
are the recovery boundary.

Before shared-host parallelism is supported, remove/parameterize container names,
allocate host ports and networks, and regenerate all IP-dependent ENRs/configs.
Do not partially implement this and claim concurrent-stack support.

## 6. Checks, time, and activation evidence

Readiness is a lifecycle stage, not acceptance success. It requires responsive
EL/CL APIs, correct identity, and initial progress for needed services. TCP-open
or a healthy container alone is insufficient. A wrong chain is a setup error;
dependent checks become blocked. Checks begin only once their prerequisites hold.

Use monotonic time for timeouts and actual chain timestamps/blocks for activation.
Keep the run-wide timeout separate from build, setup, readiness, check, and cleanup
budgets. Reserve bounded diagnostic/cleanup time before the CI job deadline.

| Check | Contract and distinguishing evidence |
| --- | --- |
| `chain_id` | Typed expected identity versus observed RPC value. Recheck in attach mode. |
| `head_progress` | Capture a baseline after its start condition; require a configured increase before the deadline. A large but frozen height fails. |
| `heads_converge` | Require lag within threshold and equal hashes at a common sampled height. Never require independently sampled latest hashes to match. |
| `head_fresh` | Sample timestamp age through a configured observation duration. Treat missing samples and invalid/future timestamps explicitly. |
| `safe_head_progress` | Observe `optimism_syncStatus`/safe tags across a baseline; allow a separate derivation budget. Unsafe progress is not proof of DA/derivation. |
| Boundary/cadence checks | Retain adjacent block identities and timestamps across the resolved activation. Use chain timestamp precision, including subsecond metadata where needed, not noisy wall-clock RPC arrival intervals. |
| Later: transfer/receipt propagation | Submit once with a dedicated ephemeral funded account; compare successful receipt/block identity on required nodes. Never retry submission blindly. |

Eventually checks pass on a qualifying observation completed within the deadline.
Transient RPC errors may be retried within that budget and remain visible.
Throughout checks must cover the whole configured window; an unavailable sample
cannot silently count as a pass. Record the achieved sampling cadence, maximum
gap, and sample count. Polling establishes sampled behavior, not continuous proof.

Use one bounded sampler per endpoint/needed observation family when checks share
reads. Preserve sequence numbers and monotonic offsets. Limit buffers and bound
requests; avoid an elaborate general event bus. Independent checks may continue
after another fails, but a failed prerequisite blocks only its dependents. Keep
state-changing actions serialized per account and separate from read-only checks.

### Fork boundaries

Resolve a fork boundary from verified generated configuration, not a TOML label.
Start observation as soon as endpoints are available, then evaluate checks when
readiness permits. Persist last inactive and first active canonical block
number/hash/timestamp, plus the feature-specific evidence the check can obtain.
Crossing the scheduled timestamp alone must be labelled **boundary crossed**, not
**fork implementation proven correct**. Add a behavior-specific check to prove
the intended rule change.

Before-fork checks must obtain their full required observations before the
boundary. If startup was late, report `error: missed_window` and block dependent
checks; never reinterpret post-fork samples as pre-fork evidence or silently move
the schedule. Historical RPC backfill may support checks explicitly defined over
historical blocks, but cannot prove past live availability.

Revalidate sampled block hashes when comparing nodes. Record reorgs and displaced
boundaries; a hash from an orphaned block must not satisfy a canonical continuity
check. Define whether each check restarts a bounded comparison or fails; never
reset the overall deadline or progress baseline indefinitely.

The default verifier delay is 15 L1 blocks at 12-second slots. Keep derivation in
a slower scenario, or explicitly configure a smaller confirmation depth and show
that change prominently in the report. Do not reduce it invisibly to make CI pass.

### Configurable L1 forks: required extension

Future illustrative syntax, rejected until the capability exists:

```toml
[devnet.l1.forks.glamsterdam]
activation_epoch = 4
```

Work required before enabling this:

1. Extend the pinned `base/optimism` generator with a typed schedule interface
   used consistently by its EL template, CL config, and beacon-genesis generation.
   Do not patch generated JSON after `genesis.ssz` has already been produced.
2. Define fork-specific EL/CL pairing, prerequisite order, blob schedules, and
   supported epoch geometry. Derive EL activation time from the resolved beacon
   epoch and genesis; reject contradictory time/epoch inputs rather than rounding.
3. Confirm and pin compatible Reth, Lighthouse, and beacon-genesis-tool revisions.
   Add a verified capability contract/version to the generator if necessary; a
   name in a Rust enum is not proof a client implements the fork.
4. Test default output compatibility for existing Compose and system tests, and
   scheduled activation for both sides of the boundary. Update the generator pin
   in `Dockerfile.devnet` only after that external change is available.
5. Validate emitted EL genesis, beacon config/state, runtime beacon spec, and
   execution observations. Enable the TOML variant and a real fork scenario only
   once behavior-specific probes exist.

Existing L2 transitions can ship before this work. Keep an unsupported-Glamsterdam
case in invalid-config fixtures, not an apparently runnable green/skipped suite.
Runtime upgrade-contract mutation is a later typed action, distinct from
provision-time scheduling and never permitted by attach mode.

## 7. Result model and failure policy

Use a versioned, serializable result model as the sole input to all renderers.
Do not scrape stdout or infer status from log text. Write atomic progress/result
snapshots so a supervisor can recover partial outcomes after a process failure.

Essential fields:

| Record | Required information |
| --- | --- |
| Run | Schema version, run ID/attempt, tested merge SHA, PR head/base SHA when applicable, timestamps, duration, scenario-selection/config digest, policy (advisory/required). |
| Scenario | Stable ID/source, resolved config, stage results, checks, services, boundary observations, start/end/duration. |
| Stage | Validate/build/setup/readiness/checks/collect/cleanup status and bounded error details. |
| Check | ID/kind, terminal status/reason, expected and actual typed values, baseline/window, sample/error counts, first/last/worst observations, evidence references, suggested next step. |
| Service | Role, actual image ID/digest, start/ready/exit times, health and exit code, sanitized endpoint, relevant evidence references. |
| Timeline | Monotonic offsets, observed head samples, gaps/errors, requested schedule and observed boundary markers kept distinct. |
| Artifact | Safe relative path, media type, bytes, truncation/redaction metadata, optional content digest. |

Suggested status policy:

| Status | Meaning |
| --- | --- |
| `passed` | Required observations satisfy the contract. |
| `failed` | Evaluated assertion did not hold, including an eventually deadline with insufficient progress. |
| `error` | Configuration/provisioning/observation/harness failure prevented valid evaluation; reason distinguishes startup timeout, malformed result, missed window, etc. |
| `blocked` | A prerequisite failed; this check was not evaluated. |
| `cancelled` | External interruption prevented completion. |
| `skipped` | Explicitly not selected; never a substitute for a required unsupported check. |

All checks in a selected scenario are required initially. No optional/xfail
policy in version one. Nonempty expected scenarios must all produce exactly one
valid result and every required check must pass. Empty, missing, duplicated,
malformed, stale, blocked, skipped-required, cancelled, and cleanup-failed runs
are non-success. Derive aggregate counts from records; never trust input summary
counts. Show simultaneous infrastructure errors and assertion failures, rather
than hide one behind a single precedence label.

Proposed CLI exits: 0 complete pass; 1 evaluated acceptance failure; 2 invalid
configuration/usage; 3 incomplete/infrastructure/harness/cleanup error. Preserve
the full result when mixed failures occur. Report rendering success does not
change the test verdict. Signal interruption remains visible as cancelled.

## 8. Visual, actionable reporting

### Recommended experience

Use a **failure-first report**, with full-width charts inside scenario details.
The first screen should answer: what failed, whether the system was actually
tested, where it failed, and what to inspect next.

```text
BASE ACCEPTANCE             FAILED + INFRASTRUCTURE ERROR
Tested revision • scenario selection • elapsed time • advisory policy

Passed 8     Failed 1     Error 1     Blocked 2

Scenario          Setup   Ready   Checks   Cleanup   Result
Smoke              OK      OK       OK       OK      Passed
Denim transition   OK      OK       FAIL     OK      Failed
Derivation         ERROR   --       --       OK      Error

▼ Denim transition / validator-catches-up
  Expected: lag <= 5 blocks and equal canonical hash at common height
  Observed: lag 14 blocks at deadline; shared-height hashes matched
  Next: inspect validator derivation/engine logs for the recorded window
  Evidence: sampled heads, RPC errors, timestamped validator log excerpt
  Reproduce: command + tested revision + resolved scenario

  [Head progress chart: builder and validator; visible gaps]
  [Expected activation and observed boundary separately labelled]
  [Service/stage timeline and accessible data table]

▶ Passed checks
▶ Resolved environment, provenance, artifacts, and limitations
```

Synthetic numbers above illustrate layout, not actual test execution.

Use a failure-first hierarchy with large charts, real fixture-derived counts,
and the typed checks above. Keep the report in one offline document without
additional navigation. Illustrative fork numbers are not configuration advice;
only verified scenario configuration should supply activation boundaries.

Actual charts must measure block lag vertically at the same observation time,
distinguish observation delay from chain activation, and leave gaps for absent
samples. Sort unsuccessful scenarios before passed ones and label each stage
with status text, not only colored glyphs.

### Required surfaces

1. **Verdict and counters:** distinguish assertion failure from startup/harness
   failure. Include scenario/check totals and incomplete outcomes; no misleading
   pass percentage excluding blocked checks.
2. **Scenario lifecycle matrix:** build/setup/readiness/checks/cleanup with icon,
   text, duration, and anchors into evidence. Failed/error scenarios first; passing
   details collapsed. Blocked tests retain their failed prerequisite reference.
3. **Failure cards:** expected versus observed, threshold/window, service, first
   failing observation, bounded logs/RPC evidence, next inspection step, and exact
   inert reproduction command. Label possible explanations as hypotheses. Do not
   claim root cause from correlation or invent automatically diagnosed remedies.
4. **Head progress and timing:** fixed inline SVG head chart and stage/service
   lanes, not a general chart engine. Show gaps, sampling cadence, reorgs, units,
   and separate expected/observed boundary markers. Never infer a running/healthy
   interval from absence of samples or connect lines across missing data.
5. **Environment/evidence drawer:** tested image IDs, resolved fork schedule,
   altered defaults, generator revision, configuration digest, evidence inventory,
   truncation/redaction notices, and provenance needed to reproduce.

Implement semantic static HTML, embedded CSS, native `<details>`, anchors, and
fixed SVG; no JavaScript is required for the initial report. Provide adjacent
text tables for charts. It must work from `file://` offline, at 320px width and
200% zoom, and using only a keyboard. Color supplements labels/icons; use WCAG AA
contrast and visible focus. Make a print layout expand failure evidence.

### PR comment

One marker-owned comment (`<!-- acceptance-results -->`) per PR. A green run gets
a short verdict and collapsed complete results; unsuccessful runs expose the
scenario summary and collapsed per-check evidence/reproduction. Include all test
IDs/statuses when within the size budget, and an explicit omitted-count/artifact
pointer otherwise. Use the same result model as HTML, not a separate verdict.

The comment links to the provider's verified run/artifact page and explains how
to download the bundle and open `report.html`. A Depot artifact is not a publicly
hosted HTML page; do not invent a preview URL. Publishing a hosted report is a
separate, later decision.

### Safety, determinism, and limits

- Render all strings as escaped text, including scenario-authored descriptions,
  remediation, Markdown delimiters, log data, and reproduction commands. No raw
  HTML, executable JSON, arbitrary inline SVG, remote fonts/scripts, or CDN calls.
- Use a restrictive CSP and allow only intended styles; no script execution.
  Artifact references are relative and contained within the report root; reject
  traversal, absolute paths, and escaping symlinks. Validate external link schemes.
- Never collect sensitive environment wholesale. Whitelist persisted fields and
  redact at collection, before writing/uploading, not just while rendering.
  Public devnet keys are still unnecessary report content; real credentials must
  never enter the artifact boundary. Unknown text can defeat heuristic redaction,
  so prefer structured allowlisted evidence and limit raw log exposure.
- Start with explicit configurable implementation constants: 64 KiB inline
  failure excerpts, 2 MiB stored per service log, 20 MiB report bundle, 50 KiB PR
  body. These exclude separate image-transfer artifacts. Bound samples and parser
  input as well as rendered output; preserve first/last/failure-adjacent evidence
  and disclose omission. Tune from measurements, never silently drop failures.
- Render deterministically from persisted timestamps/data. No render-time clock,
  random IDs, locale variation, or network requests. Identical result inputs must
  yield byte-identical HTML/Markdown. Avoid hash/self-size circularity in the
  result: an optional bundle manifest can describe finalized output files.

## 9. Depot workflow and PR publication

Add a dedicated `.depot/workflows/acceptance.yml` rather than enlarging the system
test job. Begin with manual and same-repository PR triggers; later add merge-group
execution with the same stable aggregate check name and no PR-comment assumption.
Use the tested PR merge SHA or merge-group SHA, not only the PR head. Record both
the tested SHA and PR head SHA so reporting freshness is not confused with code
provenance.

### Job structure

1. **Validate/select:** validate scenario IDs and resource bounds, emit a nonempty
   expected-scenario manifest. Initial selection is a reviewed small list; do not
   execute arbitrary matrix JSON in shell. Include run/attempt/tested SHA.
2. **Execute:** one isolated 16-core Depot runner per scenario, `fail-fast: false`.
   Build/load the PR `base:local`, `op-batcher:local`, and `devnet-setup:local-v2`
   images; explicitly pull needed external images and record their digests. Reuse
   Depot's persistent Bake cache and existing setup action. Run checks, collect
   diagnostics, stop owned resources, and upload a bounded result bundle with
   `always()`-style best-effort handling and a job-level hard timeout.
3. **Aggregate/report:** consume the expected manifest plus all available result
   artifacts. Validate identity/schema/bounds; synthesize incomplete/error entries
   for missing scenarios. Re-render aggregate HTML/Markdown, upload the report,
   write the step summary, then enforce the truthful aggregate conclusion.
4. **Comment:** a distinct, minimal-permission publisher upserts the PR comment
   from validated data using trusted code. No checkout or execution of PR code in
   a job with comment-write credentials.

Initial image strategy: build/load within each scenario runner; this is simplest
for the first one or two scenarios and benefits from warm remote layers. Measure
before adding an upstream shared-image job. For a larger matrix, compare that
against building once and transferring compressed Docker-loadable archives
(verify the chosen archive format with `docker load`). Cross-job images are not
implicitly shared. A bundle manifest must bind image IDs, architecture, profile,
tested SHA, run and attempt; verify after loading. Never push PR images to GHCR as
an incidental testing step.

Mirror the existing dev-profile build settings, including optional proof ELF
behavior, instead of accidentally compiling the full proving stack. Add
`COPY acceptance ...` to the Rust Docker workspace inputs when the library joins
Cargo. Verify all image targets still resolve the workspace. No unrelated setup
action modernization is needed.

### Trust and freshness

- Execution jobs need only read access to repository content and the narrow
  Depot build/cache capabilities actually required. No comment/package/deploy
  tokens; avoid persisted checkout credentials. Inspect provider-injected
  credentials during the CI spike rather than claiming the sandbox has none.
- Treat PR code, TOML, result JSON, logs, and generated HTML as untrusted. A result
  envelope binds provenance but is not a cryptographic attestation of honest
  execution; malicious PR code can fabricate output. Branch review/protection is
  still required.
- Build the privileged renderer/publisher from a reviewed base/default-branch
  revision or pinned artifact, not from the tested PR. A first rollout may publish
  artifacts only until trusted reporter code is on the base branch. Never execute
  binaries/scripts downloaded from the test job in the publishing job.
- Paginate comments, match the exact acceptance marker and expected bot author,
  and update only that comment. Use file-based API bodies; do not interpolate
  untrusted Markdown into shell commands. Do not modify the IAI comment.
- Serialize publication per PR separately from expensive execution. Immediately
  before writing, compare current PR head/base and the newest applicable
  run/attempt. Reject stale attempts, superseded pushes, and outdated merges.
  Include tested revision/run identity in the body. API reads/writes are not an
  atomic compare-and-swap; test races and prefer no initial 'running' comment to
  minimize stale overwrites. Concurrency cancellation alone is insufficient.
- Use actual Depot-supported run/artifact links. Existing benchmark construction
  of GitHub Actions URLs is a precedent to verify, not proof those URLs work here.
- [Depot compatibility documentation](https://depot.dev/docs/ci/compatibility)
  currently excludes fork-triggered PR workflows. Keep fork execution/publication
  a separate GitHub Actions design; never use `pull_request_target` to execute fork
  code with credentials.

### Rollout and gating

Advisory means the aggregate is not required by branch protection, not that the
runner always exits zero. Collection/reporting must still run after failed tests.
Avoid path-filtering a required workflow out of existence. When gating is enabled,
an always-present aggregate handles a deliberate not-applicable policy explicitly.
Include checker/scenario/Compose/generator/build/workflow changes in any future
selection policy, not only node Rust changes.

Cancellation and machine loss may prevent final artifact uploads. The aggregate
must handle missing results when it runs; a cancelled entire workflow remains
cancelled/incomplete, never passed. No promise of a final comment on every hard
runner loss. Re-runs use new attempts and cannot reuse stale scenario artifacts.

## 10. Delivery milestones and implementation file targets

Each milestone should be reviewable independently. Reporting is part of the
first useful slice, not an optional final polish phase.

| Milestone | Work / primary files | Exit criteria |
| --- | --- | --- |
| 1. Contracts and preview | `acceptance/Cargo.toml`, `src/{config,result,report}.rs`, report CSS/fixtures, `bin/acceptance`, root Cargo inputs, Docker workspace copy | Strict schema/result validation; synthetic pass/fail/startup-error reports render offline and are visually inspected. Unsupported forks fail before Docker. |
| 2. Read-only vertical slice | `src/{runner,check,cli}.rs`, `smoke.toml`, local Just module | Attach to an existing devnet; identity/progress/common-height convergence; JSON/HTML/Markdown agree; never changes running services. |
| 3. Owned Compose lifecycle | `src/provision.rs`, existing base/HA Compose bind roots, Bake setup target if needed, `environment.json` | Fresh smoke run from one command; current PR images proven; pre-existing developer net preserved; startup failure and cancellation produce truthful partial reports. |
| 4. Fork and derivation scenarios | `denim-transition.toml`, `derivation.toml`, temporal/boundary checks | Measured pre/post windows, no missed-window false pass; verified generated schedule and observed boundaries; slow safe-head checks have explicit budgets. |
| 5. Depot advisory | `.depot/workflows/acceptance.yml`, trusted publisher/aggregation support under `acceptance/`, docs | Same-repo PR smoke, artifacts, summary and one safe collapsible comment; injected missing/stale/failed results stay non-green. |
| 6. L1 schedule support | External `base/optimism` generator, pinned tool/client updates, `Dockerfile.devnet`, typed L1 config and scenarios | EL/CL/beacon-genesis agree; compatible clients verified; real transition behavior demonstrated before enabling Glamsterdam or another new fork. |
| 7. Broader coverage and gating | Typed transfer actions, HA scenario, CI selection/merge-group trigger | Measure reliability/cost, then enable required aggregate only with explicit maintainer approval. |

Milestone 6's generator/client investigation can start alongside milestones 2–5;
the numbering is not a requirement to postpone that external dependency. The
first useful delivery is milestones 1–3 together, including a visual report.

Parallel implementation ownership after milestone-one contracts settle:

- Provisioning owner: Compose/generated-config/cleanup; no report edits.
- Check owner: observations, time windows, fork probes, scenario files.
- Reporting owner: semantic HTML/CSS/SVG and Markdown against fixed fixture schema.
- CI owner: workflow/image provenance/aggregation/publication after lifecycle and
  result contracts exist. Coordinate shared Cargo/result changes with one owner.

No branch-protection changes, pushes, deployments, remote generator changes, or
new scheduled workflows are authorized by this plan alone.

## 11. Verification strategy

### Deterministic code tests

- Parser rejects misspelled fields, empty suites, duplicate IDs, unknown forks,
  unsafe paths, invalid durations, and unsupported versions before spawning.
- Ambient `L2_BASE_DENIM_BLOCK=99` cannot override scenario block 150. Explicitly
  disabled forks do not inherit env defaults. Resolved output matches generation.
- Independently derived schedule fixtures cover pre-Denim, exact boundary, valid
  post-Denim offsets, invalid non-divisible offsets, and EL/CL mismatch. Do not
  compute expected values by calling the production resolver.
- Paused Tokio clock and mocked RPC responses exercise both sides of deadlines,
  mid-request timeout, frozen high head, gaps, missed pre-window, reorgs, and a
  same-height/different-hash fork. Ordinary unit tests do not launch containers.
- Prefer `mockall::automock` for internal observation traits; document the reason
  if cross-method ordering/in-flight mutation requires a hand-written fake.
- Throughout checks cannot pass with zero samples or an unobserved outage.
  Eventually checks preserve transient errors even when they later pass.
- Attach mode invokes no Docker/mutating API. Transfer actions later prove no
  duplicate submission on ambiguous RPC failure and no shared-account nonce race.
- Aggregation rejects missing/duplicate/wrong-SHA/wrong-attempt/malformed results;
  startup errors block checks; cleanup failure remains non-success.

### Rendered report tests

- Fixtures: all green, assertion failure, startup error with blocked checks,
  cancelled/partial, no results, mixed failure/error, reorg, missing samples,
  oversized/truncated evidence, and long/hostile labels.
- Assert independent expected totals and visible expected/observed values, not
  only snapshots of generated markup. Same input renders byte-identically.
- Test HTML/Markdown escaping, control characters, credentials, URL schemes,
  traversal/symlinks, report-size budgets, and no remote resource requests.
- Render fixtures in an actual browser with networking disabled. Capture and
  inspect desktop and mobile screenshots, including expanded failure/startup
  error states. Check keyboard focus, details toggles, 200% zoom, local overflow,
  readable charts, and text alternatives. Generated design images are not tests.

### Real local devnet tests

- Run fresh smoke and confirm actual tested-image provenance and validator
  propagation. Run an intentionally impossible assertion and inspect the report.
- Exercise setup failure, unavailable image, readiness timeout, wrong chain,
  late boundary, SIGINT/SIGTERM, and cleanup failure. Verify no false green.
- Compare default `docker compose config` before/after mount parameterization to
  ensure developer defaults remain unchanged. Prove unrelated devnet data and
  containers survive rejected startup and acceptance cleanup.
- Confirm canonical common-height hashes, not just matching block numbers. Test
  safe-head checks independently of unsafe production and with configured delay.

### Depot spike and promotion criteria

Run cold/warm same-repo PR, rerun-attempt, superseding-push, and merge-group cases.
Verify artifact action support, canonical URLs, permissions, image loading, and
what actually happens under cancellation. Before wider rollout, record:

- build/cache/startup/test/report durations and p50/p95 once enough samples exist;
- peak RAM/disk, report/image artifact sizes, transfer/load time, runner minutes;
- zero unaccounted expected scenarios in successful runs;
- zero owned resources left after graceful completion/failure/cancellation tests;
- flake/retry rate, reason distribution, and percentage of failures with useful
  evidence and reproduction instructions.

Propose gating budgets from observed data; do not invent a runtime or reliability
claim. Do not use automatic reruns to hide first-attempt failures: show both.

## 12. Remaining decisions requiring evidence

1. Compatible generator/EL/CL/toolchain versions and behavior probes for each new
   L1 fork, especially Glamsterdam. This gates that scenario, not the core runner.
2. Whether the full single-sequencer Compose stack fits the desired PR budget;
   measure before removing observability/shadow services or inventing a new stack.
3. Build-per-runner versus shared image artifacts once the matrix grows.
4. Depot artifact limits/URLs, injected credentials, cancellation guarantees, and
   trusted publisher bootstrap. Validate in an authorized CI spike.
5. Final timeouts and promotion threshold for required merge-queue acceptance.
6. Whether fork PR acceptance or public HTML hosting is wanted later; neither is
   necessary for same-repository advisory CI and downloadable visual reports.
