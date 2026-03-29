# Continuous integration

This guide summarizes how GitHub Actions workflows relate to local [`just`](https://github.com/casey/just) recipes. It does not change CI behavior; workflow definitions remain the source of truth under [`.github/workflows/`](../../.github/workflows/).

**TL;DR:** GitHub runs many jobs in parallel and **never** applies `just fix` / auto-fixes. Locally, `just check` only verifies; `just ci` runs **`fix` first** (mutates files) then checks, tests, lychee, zepter, and no-std scripts. **`just check` / `just ci` use `check-clippy` and `test` (or `test-affected` in `just pr`) without `--locked` or the `ci` Cargo profile**—the **Clippy** and **Test** jobs in [`ci.yml`](../../.github/workflows/ci.yml) use **`just check-clippy-ci`** and **`just test-ci` / `just test-affected-ci`** instead. See the [Gaps](#gaps-for-local-versus-github-ci) section and the mapping table. Contributor setup remains in [`CONTRIBUTING.md`](../../CONTRIBUTING.md).

## Local recipes

### `just ci`

Defined in the root [`Justfile`](../../Justfile) as:

`fix` → `check` → `lychee` → `zepter` → `check-no-std` → `check-no-std-proof`

- **`fix`** runs `build-contracts`, `format-fix`, `clippy-fix`, and `zepter-fix`. That **modifies** the working tree (formatting, Clippy auto-fixes, Zepter feature formatting). Run it only when you intend to apply fixes.
- **`check`** runs `check-format`, `check-udeps`, `check-clippy`, **`test`** (full workspace via `cargo nextest`, all features, excluding `devnet`), and `check-deny`.

So `just ci` is a **local convenience bundle**: it is **not** a single job in GitHub Actions, and it **does not** run every CI job (see gaps below).

**CI vs local:** Workflows only run **check**-style commands (format check, clippy with `-D warnings`, tests, etc.). They do **not** run `format-fix`, `clippy-fix`, or `zepter-fix`. If CI fails on format or clippy, run `just fix` or the specific `check-*` recipe locally, commit the diff, and push again.

### `just pr`

`fix` → `check-format` → `check-udeps` → `check-clippy` → `check-deny` → `lychee` → `zepter` → `check-no-std` → `check-no-std-proof` → **`test-affected`**

Compared to `just check`, `just pr` swaps the full workspace **`test`** for **`test-affected`** (crates changed vs `main` by default), similar in **scope** to CI’s PR tests. CI still uses **`just test-affected-ci "origin/$BASE_REF"`**, which adds **`--locked`**, **`--cargo-profile ci`**, and nextest exit-code handling—run that locally if you need a closer match than `just pr`.

### Other useful recipes

| Recipe | Purpose |
|--------|---------|
| `just check-clippy-ci` | Clippy with `--locked` and `--profile ci` (matches the **Clippy** job in [`ci.yml`](../../.github/workflows/ci.yml)). |
| `just test-ci` | Full workspace tests with `--locked` and `--cargo-profile ci`. |
| `just test-affected-ci base` | Affected crates only; same profile/lock semantics as CI’s PR test path. |
| `just build-ci` | `cargo build --locked --workspace --all-targets --profile ci`. |
| `just devnet pull-images` then `just devnet tests-ci` | Matches the **Devnet Tests** job in `ci.yml` (Docker). |
| `just actions::test` | Action tests (protocol actors); used by [`action-tests.yml`](../../.github/workflows/action-tests.yml). |

## Workflow files (overview)

| Workflow | When it runs (summary) | What it does |
|----------|-------------------------|--------------|
| [`ci.yml`](../../.github/workflows/ci.yml) | `push` **only** to `main`; any `pull_request`; `merge_group` | Primary Rust CI: many parallel jobs (lockfile, build, tests, musl, fmt, clippy, optional Docker bake, bench smoke, udeps, crate-deps, deny, devnet). **Pushes to other branches without an open PR do not run this workflow.** See the mapping table below. |
| [`action-tests.yml`](../../.github/workflows/action-tests.yml) | Same `on:` shape as `ci.yml` (`push` to `main`, any `pull_request`, `merge_group`; no path filter) | `just actions::test` — in-process protocol action tests. |
| [`no-std.yml`](../../.github/workflows/no-std.yml) | Same `on:` as `ci.yml` (`push` to `main`, any `pull_request`, `merge_group`) | Two jobs: stable **`check-no-std.sh`** (riscv32 `no_std` / `--no-default-features` list) and nightly **`check-no-std-proof.sh`** (`-Zbuild-std`, requires `rust-src`). |
| [`zepter.yml`](../../.github/workflows/zepter.yml) | `push`/`merge_group` to `main`; `pull_request` **only when the PR targets `main`** | `just zepter` — Cargo feature manifest checks. |
| [`lychee.yml`](../../.github/workflows/lychee.yml) | `push` (except certain branches), `pull_request`, `merge_group`, `workflow_dispatch` | Link checking with [`lychee`](https://github.com/lycheeverse/lychee) and [`lychee.toml`](../../lychee.toml) (action uses the same args as local `just lychee`). |
| [`docs-specs-ci.yml`](../../.github/workflows/docs-specs-ci.yml) | `push` to `main` or `pull_request` when **`docs/specs/**`** changes; `workflow_dispatch` | In `docs/specs`: `bun ci` then `bun run build` (Node 22 + Bun 1.2). |
| [`benchmark.yml`](../../.github/workflows/benchmark.yml) | `push`/`pull_request`/`workflow_dispatch` | External Go benchmark harness against a pinned repo. The **`build-binaries`** job is **disabled** (`if: false`), so the **`benchmark`** job (which `needs` it) does not produce a working pipeline until that is reverted—read the workflow before relying on it. |
| [`docker.yml`](../../.github/workflows/docker.yml) | `push` to `main`, `workflow_dispatch` | Builds and pushes multi-arch node images to GHCR (separate from the optional Docker job inside `ci.yml`). |
| [`build-release.yml`](../../.github/workflows/build-release.yml) | **`workflow_call` only** | Invoked by release automation; builds release binaries and related artifacts. |
| [`publish-release.yml`](../../.github/workflows/publish-release.yml) / [`start-release.yml`](../../.github/workflows/start-release.yml) / [`create-rc.yml`](../../.github/workflows/create-rc.yml) | Manual / release-branch automation | Release versioning, RC tags, publishing. See [`RELEASE.md`](RELEASE.md). |
| [`stale.yml`](../../.github/workflows/stale.yml) | `schedule` + `workflow_dispatch` | Stale issue/PR labeling and related housekeeping. |
| [`claude-review.yml`](../../.github/workflows/claude-review.yml) | `pull_request` (selected activity types) | **Claude Code Review** on same-repo PRs only (`if` skips forks); expects runner group `BaseRunnerGroup`. |

## `ci.yml` jobs ↔ `just` commands

| `ci.yml` job | Typical `just` / command equivalent |
|--------------|--------------------------------------|
| **Lockfile** | `cargo metadata --format-version 1 --no-deps --locked` |
| **Build** | `just build-ci` |
| **Test** | `pull_request`: `just test-affected-ci "origin/$BASE_REF"` (after `git fetch` of the base branch). Pushes to `main`, `merge_group`, and other non-`pull_request` events: `just test-ci`. |
| **Test SIGSEGV (musl)** | `cargo test --locked -p base-cli-utils --test sigsegv_test --target x86_64-unknown-linux-musl` (after musl toolchain setup) |
| **Format** | `just check-format` |
| **Clippy** | `just check-clippy-ci` |
| **Docker** (conditional) | `docker buildx bake -f etc/docker/docker-bake.hcl client --load` |
| **Benchmarks** | `cargo bench --locked -p base-proof-mpt --bench trie_node -- --test` |
| **udeps** | `just check-udeps` |
| **Crate Dependencies** | `just check-crate-deps` |
| **Cargo Deny** | `just check-deny` |
| **Devnet Tests** | `just devnet pull-images` then `just devnet tests-ci` |

## Gaps for local versus GitHub CI

The following items are **not** covered by **`just ci` alone** (and some are easy to forget because separate workflows cover them on GitHub). Use this list when you want parity with the full matrix:

- **Lockfile-only** validation (same as CI: `cargo metadata --format-version 1 --no-deps --locked`).
- **Clippy / tests with CI flags** (`just check-clippy-ci`, `just test-ci`, or `just test-affected-ci …`)—`just ci` uses `check-clippy` and `test` instead.
- **`build-ci`** full workspace build with the `ci` profile.
- **musl SIGSEGV** test binary.
- **Devnet** Docker-based tests as in `ci.yml`: **`just devnet pull-images`** then **`just devnet tests-ci`** (not only the latter).
- **Action tests** (`just actions::test`).
- **Docker image** bakes (`ci.yml` conditional job or `docker.yml`).
- **`check-crate-deps`** (crate boundary script).

Use **`just ci`** for a broad local sweep (with the caveat that **`fix` edits files**). For a closer match to what runs on a PR, also run the missing pieces above or rely on the green check suite on GitHub.

## Shared setup

Most Rust workflows use [`.github/actions/setup`](../../.github/actions/setup) to install the toolchain, optional components (`clippy`, `rustfmt`, `rust-src`), mold, Foundry, `cargo-nextest`, and caches. Details are in that action’s `action.yml`.
