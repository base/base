# Testing Overview

This guide explains the checks that run before code ships to Base: what runs on your machine,
what runs in CI, and which `just` command maps to which check. It complements
[CONTRIBUTING.md](../../CONTRIBUTING.md) (contribution workflow) and [CLAUDE.md](../../CLAUDE.md)
(code style and test conventions).


## Quick Start

Before opening a pull request, run:

```sh
just ci
```

This is the single command [CONTRIBUTING.md](../../CONTRIBUTING.md) asks every contributor to run.
It fixes formatting/clippy/zepter issues, then runs every check and the full unit test suite:

```
fix → check::all (format, clippy, udeps, deny) → test → lychee → zepter → check::no-std → check::no-std-proof
```

If you only touched a few crates and want faster feedback, `just pr` runs the same checks but
scopes clippy and tests to crates affected by your change (relative to `main`):

```sh
just pr
```

Both commands require Docker, [`just`](https://github.com/casey/just), and Foundry (`forge`) to be
installed — see the Developer Setup section of [CONTRIBUTING.md](../../CONTRIBUTING.md).


## The Four Testing Tiers

Base tests protocol behavior at four levels, trading off speed against how much of the real stack
is exercised:

| Tier | Speed | What it exercises | Where |
|---|---|---|---|
| Unit tests | milliseconds | A single function or type in isolation | Colocated `#[cfg(test)] mod tests` blocks |
| Action tests | milliseconds | Real protocol logic (batching, derivation) with in-memory actors | `actions/harness` (`base-action-harness`) |
| System tests | minutes | The full L1 + L2 stack via Docker/testcontainers | `etc/systems` (`base-system-tests`) |
| Fuzz tests | hours (nightly) | Randomized transaction streams for sync-parity regressions | `base-system-tests`, nightly only |

Each tier is described below.


### Unit Tests

Unit tests live next to the code they test, inside a `#[cfg(test)] mod tests { ... }` block at the
end of the file (see [CLAUDE.md](../../CLAUDE.md) for the exact convention). Run the full workspace
suite with:

```sh
just test
```

This runs `cargo nextest run --workspace --all-features --exclude base-system-tests --no-fail-fast`
after building test contracts and SP1 ELFs. To scope to only the crates affected by your branch:

```sh
just test-affected
```

`base-system-tests` is excluded here — Docker-backed integration tests and the crate's colocated
unit tests both run under [System Tests](#system-tests) below.


### Action Tests

Action tests are an integration-testing framework for the Base rollup protocol: L1 block producer,
batcher, sequencer, and verifier are modelled as lightweight in-memory actors, driven through a
scripted sequence of actions, with assertions on the resulting chain state. There are no real
nodes, no network sockets, and no Docker containers, but the same production types are used for
batch encoding, channel compression, and derivation — so action tests catch protocol-boundary bugs
that unit tests miss, in milliseconds rather than minutes.

Run them with:

```sh
just actions test
```

or directly:

```sh
cargo nextest run -p base-action-harness
```

See [`actions/README.md`](../../actions/README.md) for the actor architecture, how to write a new
scenario, and why action tests exist as a middle tier between unit and system tests.


### System Tests

System tests spin up an isolated L1 + L2 stack using [testcontainers](https://testcontainers.com/)
and exercise the node end-to-end — real Reth (L1), real Lighthouse, real Base sequencer/validator
processes. This is the slowest and most thorough tier. Run them locally with:

```sh
just devnet tests
```

which runs `cargo nextest run -p base-system-tests` after building test contracts. See
[`etc/systems/README.md`](../../etc/systems/README.md) for the crate itself.

System tests require Docker and are **not** run on every pull request (see
[CI Pipeline](#ci-pipeline) below) because of their cost. They run as the required
`ci / System Tests` check on the merge queue, so a commit cannot reach `main` unless they pass.


### Fuzz Tests

A nightly workflow fuzzes sync-parity behavior with randomized transaction streams, sharded across
4 parallel jobs, each with a fresh random seed (logged so a failure can be replayed):

```sh
cargo nextest run -P ci -p base-system-tests --cargo-profile ci --no-capture -E 'test(fuzz_sync_parity)'
```

This does not run on pull requests or the merge queue — only on a daily schedule (07:00 UTC) or via
manual `workflow_dispatch` with a specific seed to replay a failure.


## Other Checks

Besides tests, `just ci` / `just pr` run several static checks:

| Check | Command | Purpose |
|---|---|---|
| Format | `cargo +nightly fmt --all -- --check` | Enforces `rustfmt.toml` (2024 edition style) |
| Clippy | `cargo clippy --workspace --all-features --all-targets -- -D warnings` | Lints, warnings denied |
| Unused deps | `cargo +nightly udeps --locked --workspace --all-features --all-targets` | Flags unused `Cargo.toml` dependencies |
| Dependency bans/licenses | `cargo deny check bans --hide-inclusion-graph` | Enforces `deny.toml` (allowed licenses, banned crates, source restrictions) |
| `no_std` | `etc/scripts/ci/check-no-std.sh` | Confirms `no_std` crates still compile without `std` |
| `no_std` (proof) | `etc/scripts/ci/check-no-std-proof.sh` | Same, for bare-metal FPVM proof crates |
| Feature flags | `zepter format features && zepter` | Validates Cargo feature propagation across the workspace |
| Links | `lychee --config ./lychee.toml .` | Checks for dead links across the repo |

Each has a `just check::<name>` recipe (e.g. `just check::clippy`, `just check::udeps`) — run
`just check` to list them all. `just fix` auto-fixes formatting, clippy, and zepter issues where
possible.


## CI Pipeline

CI runs a different subset of checks depending on where a change is in its lifecycle:

| Stage | Trigger | Workflow | Scope |
|---|---|---|---|
| Pull request | `pull_request` | `ci-pr.yml` → `ci-core.yml` | Build/clippy/test scoped to **affected crates only** vs. base branch |
| Pull request | `pull_request` | `no-std.yml`, `zepter.yml`, `lychee.yml`, `action-tests.yml`, `base-std-fork-tests.yml` | Full workspace (these checks are already fast) |
| Merge queue | `merge_group` | `ci-merge-queue.yml` → `ci-core.yml` | **Full** workspace build/clippy/test, plus **system tests** |
| Push to `main` | `push` | `ci-main-cache.yml` | Warms the shared Rust build cache |
| Nightly | `schedule` (07:00 UTC) | `fuzz-nightly.yml` | Sharded sync-parity fuzzing |
| Nightly | `schedule` (13:00 UTC) | `udeps-report.yml` | Unused-dependency report, files a GitHub issue on findings |
| Release | manual / push to `releases/v*` | see [RELEASE.md](RELEASE.md) | Release builds, RC tags, Docker images |

The affected-crates scoping on pull requests (via `etc/scripts/local/affected-crates.py`) is why
`just pr` locally mirrors PR CI, while `just ci` mirrors what the merge queue ultimately requires —
every affected crate, workspace-wide, before a change reaches `main`. System tests stay on the
merge queue (not pull requests) so PR feedback stays fast; they are a required status check, so
the queue will not merge if they fail.

`action-tests.yml`, despite the name, is unrelated to testing GitHub Actions workflows — it runs
the [action tests](#action-tests) described above (`just actions::test-ci`) on every PR and
merge-queue run. Action tests are also a required status check.


## Guidelines

- Add or update tests for any behavioral change (required by
  [CONTRIBUTING.md](../../CONTRIBUTING.md)).
- Prefer the fastest tier that actually exercises the behavior under test: unit tests for isolated
  logic, action tests for protocol-boundary behavior (batching, derivation, channel encoding),
  system tests only when you need the real L1/L2 stack end-to-end.
- Keep unit tests colocated with implementation, in a trailing `#[cfg(test)] mod tests { ... }`
  block — see [CLAUDE.md](../../CLAUDE.md).
- Run `just pr` before pushing for fast feedback, and `just ci` before requesting review to match
  what the merge queue will enforce.


## Reference

| Command | Runs |
|---|---|
| `just ci` | Full local pre-push validation (fix + all checks + full test suite) |
| `just pr` | Faster affected-crates-only variant of `just ci` |
| `just fix` | Auto-fixes formatting, clippy, and zepter issues |
| `just test` | Unit tests, full workspace |
| `just test-affected` | Unit tests, affected crates only |
| `just actions test` | Action tests (`base-action-harness`) |
| `just devnet tests` | System tests (`base-system-tests`, requires Docker) |
| `just check` | Lists all `check::*` static-check recipes |
| `just lychee` | Link check |
| `just zepter` | Feature-flag validation |

For release automation (RC tags, publishing), see [RELEASE.md](RELEASE.md). For the P2P stack
being tested, see [P2P.md](P2P.md).
