# Glamsterdam acceptance

## Status and tested boundary

Both DA cases have passed locally across real scheduled activation with the pinned
Reth/Lighthouse pair, including independent repeated runs. The workflow remains
**manual**, not a required merge gate. Tooling unit tests alone are not client
qualification; the recorded live evidence below is the acceptance result.
No remote acceptance CI success is claimed.

The acceptance boundary is containerized Ethereum Reth and a real Gloas consensus
client, with Base's builder, sequencer, verifier and batcher running **in-process**
through `SystemTestStackBuilder`. This is not qualification of shipped Base
executables. Base's L2 Reth dependency and L2 execution rules remain unchanged.

Scenarios belong to the grouped `base-system-tests` integration-test target
`tests/acceptance/main.rs`. They use ordinary Rust setup → action → bounded wait →
assertion code, not simulated actors, manual mining, or fabricated safe/finalized
heads. Calldata and blob are independently selected cases of the same scenario.

## Local qualification record — 2026-09-18

The complete recorded runner was exercised from clean source
`ee5bdd323f9f695647f487b63b6fad0454886667` on macOS arm64 with Podman's Docker API,
Rust 1.96.0 and nextest 0.9.144. This record is a documentation-only follow-up.
Both cases used the same compiled acceptance binary, SHA256
`d8ae3c7a0fb35b42cdd03c5fb0c7cd61c7770ec3e37f95361839060678b92784`.

| Case | Test duration | Matching safe L2 blocks | Attributed L1 batches |
| --- | --- | --- | --- |
| Calldata | 538.78 s | 6 before, 195 after | Type 2, blocks 4 and 66 |
| Blob | 532.20 s | 6 before, 196 after | Type 3, blocks 4 and 68; actual blobs authenticated |

Both cases authenticated the L1 boundary at execution blocks 63/64, observed
Gloas finalized epoch 9, successful transfer balance deltas of exactly 1,337 and
2,003 wei, and matching sequencer/verifier receipt-block hashes after safety.
L1 SLOTNUM returned slot 64; both L2 nodes rejected it as `NotActivated`.

Post-fork safe hashes:

- Calldata block 195: `0x0b7195e11d1f75763edf49cc9ceabf8fcf25e27e7b03593cfc06bb21c3198568`.
- Blob block 196: `0x082209370623259e7a8a54ed0888ed2e64fff567e9cac89ba2ae401c894b3b51`.

Actual arm64 image identities:

- Reth: `sha256:e2d89346092ffee5db87a7ab199e7ffd2390166c03932f6b289a17fbb268a714`.
- Lighthouse: `sha256:da3f81fdb0ae20fa06cfcf1fa38f80ee386d16c8f126d58b071ee0ccc652fee5`.
- Setup: `sha256:8a724319cde1d207e9d09d633393adff3d48a6945edb6e29b8f29a8e8513af2a`.

The immutable manifest digests, source revisions, executed versions and full build
commands are in each run's provenance. Local artifacts are retained under
`/private/tmp/base-glamsterdam-evidence/final-calldata` and `final-blob`, including
JUnit (one executed case, no retries), full logs, configurations, raw transaction
and blob data, and passed scenario/cleanup reports. Each mode also passed an
independent parent-slice run and an earlier recorded run with this fixed pair;
this is a small local repeatability sample, not Linux/amd64 CI qualification.

A deliberate error after setup retained diagnostics before removing all owned
resources. A separate deliberately timed-out real nextest process returned 100;
the fallback captured all three surviving L1 containers' logs/inspect data before
removing their exact IDs and one exclusive network. Another live case and
unrelated sentinel resources were preserved. These are failure-path checks, not
successful acceptance cases.

Focused Rust tests, Clippy with `-D warnings`, nightly workspace formatting and
35 Python tooling tests passed. The 46-test existing library suite passed. The
ordinary unchanged smoke path was attempted but fails on this host because
testcontainers tries to create Podman's reserved `none` network. The fixture's
separate setup network avoids that incompatibility without changing the default.
Logs retain nonfatal multiproof-worker and shutdown database diagnostics.
The early concurrent-startup investigation also observed a TCP bind failure;
no gate retry hides it. CI deliberately isolates modes on separate workers.

## Client pins and selection

[`fixtures/glamsterdam.json`](fixtures/glamsterdam.json) is the single source of
truth for the immutable client image digests and source revisions. The runner
pulls those digests and saves the manifest alongside actual local Docker image
IDs, architecture and available OCI source/version labels.

Reth v2.6.0 is a candidate because its source includes the Amsterdam payload-field
validation following [the upstream validation work](https://github.com/paradigmxyz/reth/commit/6763b2a2d07ec7f2c51df5bcc3c4f94d741b124b).
Lighthouse is pinned to source `8459a76775f2a7c8b11de5a8ab8c7eb473cd622c` and
an immutable image digest, not a moving tag. It contains the complete upstream
[Gloas blob retrieval fix #9937](https://github.com/sigp/lighthouse/commit/309df481aa70136bc1f4c4dae1663b8f4287af72).
The stable v8.2.2 candidate passed L1 qualification and calldata, but its blob REST
handler rejected real Gloas blocks with `400: block is pre-Deneb and has no blobs`:
it used the removed commitment field and legacy sidecar reconstruction. That
candidate is **not** qualified for blob DA; the test exposed rather than bypassed
this failure.

The pinned replacement's [publication run](https://github.com/sigp/lighthouse/actions/runs/35342595526)
records checkout of the exact source SHA, export of arm64 manifest
`ede02a435f8c34d182b23ec1282350aa2c6606ea21fb5286fa0141e15ed98079`, and publication
of index `c0a6cf874d0a0596bf6c5c4e485103ac57af7ffd7b60272b4a26a9ebd05368f9`.
The pulled binary reports `Lighthouse v8.2.2-8459a76`. These inspected build logs
and binary identity establish the selected build's provenance, not runtime
interoperability by themselves. Neither the system-test Reth v2.5.2 default nor
a mutable nightly tag is implicitly qualified.

The Base acceptance cases use the minimal preset (8 slots per epoch), 6-second
L1 slots, 64 validators and activation epoch 8: 384 seconds of pre-fork runway.
Base's existing 2-second L2 cadence is unchanged. The asynchronous origin selector
needs two L2 build ticks to prepare and adopt each successor; a 2-second L1
therefore outpaces it indefinitely. Six-second L1 slots provide service margin
rather than hiding that mismatch behind a longer origin wait. The finality budget
is four actual CL epochs (192 seconds). The separate L1-only qualification uses
2-second slots and activation epoch 8 (128 seconds).

Acceptance DA configuration flushes two-L1-block channels and polls receipts
every second; ordinary system-test polling defaults remain unchanged. The
single-validator setup fails Gloas PTC selection because some per-slot committees
are empty. The generator patch exposes `BASE_DEVNET_VALIDATOR_COUNT` while
preserving its default of 1. These are explicit test deviations, not production
timing or validator assumptions. The fixture-only offline setup container uses
an owned bridge rather than an airgapped network, for Podman compatibility.

The runner builds `devnet-setup:glamsterdam-v1` from `Dockerfile.devnet`, checks the
setup patch against the manifest SHA256, and records the resulting image ID. It
never replaces `devnet-setup:local-v2`, the ordinary system-test default.

## Local commands

Run from the repository root with Python 3.11+, Docker, Rust/native dependencies,
Just and cargo-nextest available. Use a host comparable to the 16-CPU CI worker.
The wrapper builds the setup image and the Rust test binary, and pulls immutable
EL/CL images before executing the selected case:

```sh
just devnet glamsterdam-calldata
just devnet glamsterdam-blob
```

Each command prints its fresh artifact directory. To choose an output location,
set `BASE_GLAMSTERDAM_ARTIFACTS` to a **nonexistent** path outside the checkout
(or under a Git-ignored directory). An existing directory is refused, not emptied.
Unignored in-checkout artifacts are rejected so generated evidence cannot change
the source snapshot during compilation. Unset `BASE_SYSTEM_TEST_SHARED_L1_RUNTIME` rather than
attaching a scheduled test to somebody else's chain.

The runner uses this nextest selection (shown without its per-run report-path
configuration). Build/pull images before using it directly:

```sh
RUST_MIN_STACK=33554432 cargo nextest run -P glamsterdam --locked \
  -p base-system-tests --no-default-features --test acceptance --cargo-profile ci \
  --run-ignored only --retries 0 --no-tests fail \
  -E 'test(=glamsterdam::glamsterdam_calldata)'
```

Replace the exact test name with `glamsterdam::glamsterdam_blob` for blob DA.
For a focused Cargo/libtest debugging run after image preparation:

```sh
RUST_MIN_STACK=33554432 cargo test --locked -p base-system-tests \
  --no-default-features --test acceptance --profile ci -- \
  --ignored --exact glamsterdam::glamsterdam_calldata --nocapture
```

Cargo/libtest does not produce the required JUnit gate report. Prefer the wrapper
for recorded qualification: it isolates nextest's report store, captures full
build/test output and source/image provenance, and validates results. It builds
once with `cargo nextest list --list-type binaries-only --message-format json`,
records that binary's SHA256, then uses `--binaries-metadata` and `--cargo-metadata`
to run the same binary without rebuilding after the source snapshot. Rust owns
normal cleanup. After nextest exits, the wrapper also checks exact per-run artifact
ownership labels: it captures surviving containers' full logs and inspect data,
then removes only verified container IDs and their exclusive fixture networks.
It refuses malformed ownership or unrelated network attachments, and never removes
volumes/images, prunes globally, or runs `devnet down`. Cleanup failure fails the
gate; `cleanup-report.json` is required alongside the scenario result.

Killing the wrapper itself can still bypass this fallback. After ensuring its test
process is no longer running, recover only that labelled case with:

```sh
python3 etc/scripts/acceptance/cleanup.py /path/to/case-artifacts
```

Uncertain ownership or incomplete diagnostics fails closed and may require manual
inspection of retained full IDs. Keep the printed artifacts for diagnosis.

## Acceptance contract

Each DA case must demonstrate all of the following:

1. **Real L1 activation.** Start before the fork; match the EL timestamp to the CL
   epoch schedule. Authenticate the last pre-fork and first post-fork headers and
   their parent linkage. Check `blockAccessListHash` and `slotNumber` absence and
   presence, real Gloas consensus activation, progress and finality.
2. **Real transaction effects.** Successful, identifiable transfers on both sides
   produce exact recipient balance changes, pinned to receipt block numbers and
   hashes. A responsive RPC or advancing height is not sufficient evidence.
3. **Batching and safe derivation.** Successful L1 inbox submissions have the expected
   DA transaction type. Decode batch data sufficiently to attribute the tested L2
   traffic; a transaction merely sent by the batcher is not enough. Both sequencer
   and verifier must mark the transfer blocks safe and agree on their hashes.
4. **Unchanged L2 rules.** L2 Amsterdam is unscheduled, its headers omit the Amsterdam
   fields, and `SLOTNUM` remains unactivated on L2 while working on post-fork L1.

Native/zk/TEE proving, withdrawal finalization, live deployments, restart/resync,
fallback, controlled ePBS missing-data recovery and reorgs are outside this first
scenario. Existing tests for those behaviors must remain separate.

## Reproducibility and ownership

- Record immutable client source revisions and image digests, including the actual
  local image IDs and architectures. Engine API method names alone do not establish
  compatibility. Do not substitute Geth if Reth fails.
- Build dependencies, test binaries and images **before** resolving genesis time and
  starting the fork clock. Fail if startup misses the pre-fork window; do not convert
  the test to post-fork-only operation.
- Use fresh state and unique container/network ownership per run. Do not attach the
  scheduled test to the shared-L1 fixture or use stable devnet names/ports.
- Capture component logs and rendered configs before cleanup on failure. Test a
  deliberate failure path. Cleanup must target only resources owned by that run;
  never run `docker system prune`, `just devnet down`, or delete caller-owned data.
- Record all timing/parameter differences from production, including slot duration,
  epoch length, validator count, activation epoch, batch flushing and receipt polling.
  Shortened receipt polling belongs to this fixture only.
- Retain artifacts until inspected. A successful scenario result is written only
  after both assertions and explicit cleanup succeed. Interrupted runs must not
  leave a successful result that a later run could mistake for its own.

## Machine-readable results

Each selected case owns a fresh artifact directory, communicated to Rust through
`BASE_GLAMSTERDAM_ARTIFACTS`. Do not reuse a previous directory. Required evidence
includes source/build provenance, actual EL/CL versions and identities, genesis
and rendered configuration, schedule and boundary headers, transfer receipts and
balances, attributed batch data, matched safe block hashes, and component logs.

The scenario writes `evidence.json` with schema version 1:

- `schema_version`: `1`;
- `case`: `glamsterdam::glamsterdam_calldata` or `glamsterdam::glamsterdam_blob`;
- `status`: `passed` only after assertions and cleanup succeed;
- `activation` and `l2_rules`: nonempty evidence objects;
- `transfers`, `batches` and `safe_blocks`: objects each containing nonempty `pre`
  and `post` evidence objects.

Rust owns the observable assertions and the detailed evidence. The result checker
checks artifact completeness, not consensus correctness. It also requires a
`test-results.xml` JUnit report containing exactly the selected case once, with no
skips, errors, failures or retry records. It requires nonempty provenance, client
pins, run/build/test-command logs, Cargo/nextest binary metadata, rendered
nextest/config files, RPC diagnostics, and all three L1 components' logs/identity
JSON. `source.patch` must exist but can be empty for a
clean checkout. A missing report or zero executed cases is a failure, never a
skipped success.

Validate an existing artifact bundle:

```sh
just devnet glamsterdam-check-results calldata /path/to/calldata-artifacts
just devnet glamsterdam-check-results blob /path/to/blob-artifacts
```

Test the fail-closed result checker independently (no Docker or devnet):

```sh
just devnet glamsterdam-check-tools
```

`capture-provenance.py snapshot` records the checkout revision, dirty tracked patch
hash and untracked-file hashes **before** builds. `capture` compares that source
snapshot after preparation and records the actual commands, tool versions, compiled
acceptance binary path/size/SHA256 and Docker image IDs. It also runs each pinned
EL/CL binary's `--version` in a short-lived, network-disabled container before the
fork clock starts. `verify` rejects changed source, binaries or images immediately
before execution. The runner stores attempted preparation commands even when a
pull/build fails, and records the exact nextest execution command separately.

Capture refuses to overwrite provenance. These checks bind the observed local
build and runtime artifacts; they are not a hermetic build attestation and do not
certify that a published image was built from its advertised source revision.

The script does not collect environment variables or Docker credentials. Dirty
patches and logs may nevertheless contain sensitive source or runtime data: review
artifacts before sharing. Untracked contents are not copied, so dirty source with
untracked files is not a self-contained reproducible source snapshot. Prefer a
clean committed checkout for qualification runs.

## CI promotion

The `glamsterdam` nextest profile disables retries, serializes a case's test
execution and retains successful and failed output in JUnit. A runnable gate also
needs exact-case selection, explicit `--retries 0 --no-tests fail`, fresh report
paths, and validation of scenario evidence. A profile alone is not a gate.

[`.depot/workflows/glamsterdam-acceptance.yml`](../../.depot/workflows/glamsterdam-acceptance.yml)
is explicitly dispatched with a `da` choice of `calldata`, `blob` or `both`.
Each selected mode has its own 16-CPU worker, fresh artifact directory and
fail-closed result check. One failing mode does not cancel the other. The workflow
always attempts to upload the full artifact directory, including partial failure
output, JUnit, client logs, rendered configs, provenance and structured scenario
evidence. It has no `pull_request`, `merge_group` or scheduled trigger.

Run it through the repository's Depot workflow interface after the complete stack
is available on the selected ref. Adding the workflow is not evidence that it ran
remotely or that either acceptance case passed.

Do not add the workflow to the merge queue or required checks until both modes have
repeated successful real-client runs and an exercised diagnostics-before-cleanup
failure path. Record actual source/build provenance, image identities, repeated
runs and remaining limitations when making that promotion.
