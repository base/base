# Agent Instructions

## Repository Layout

- `bin/`: thin binary crates (node, builder, consensus, basectl, proposer, challenger, prover services, and others).
- `crates/`: library crates grouped as `batcher`, `builder`, `common`, `consensus`, `execution`, `infra`, `proof`, `utilities`.
- `acceptance/`, `actions/`, `etc/systems/`: real-devnet acceptance scenarios, action tests, and system tests.
- `etc/`: Docker and Compose files, `just` modules, CI and local scripts, upstream pins, and tools.
- `docs/`: cross-cutting guides and specs. Crate and binary docs are READMEs next to their source.
- Run `just` to list recipes. [docs/guides/TESTING.md](docs/guides/TESTING.md) explains what `just ci`, `just pr`, and each check cover.

## Crate Architecture and Public API

- All crate names must use the `base-` prefix, for example `base-enclave` or `base-builder-core`.
- Keep `lib.rs` files minimal and free of logic.
- Group each module declaration with its re-export (`mod foo; pub use foo::Bar;`) instead of listing all modules and then all re-exports.
- Do not declare modules `pub` or `pub(crate)` unless they are test utilities, such as `pub mod test_utils`.
- Make all structs, types, enums, and functions within modules `pub`, and re-export them from `lib.rs`. Do not introduce private or `pub(crate)` types.
- Prefer methods on a type, including a unit struct when appropriate, over bare functions so the public API exports types rather than loose functions.
- Keep binary crates under `bin/` as minimal glue code. Put meaningful logic in library crates.

## Cargo Manifests and Features

- Sort `Cargo.toml` dependencies by line length (waterfall style) and preserve the workspace's logical grouping.
- Put features sections at the bottom of manifests.
- Every crate and binary manifest must inherit workspace lints with `[lints] workspace = true`.
- Do not enable dependency features in the workspace root `Cargo.toml`. Enable them only in the crates or binaries that need them to prevent feature leakage into `no_std` crates.

## Documentation and Lints

- Docs are READMEs next to the code they describe, plus cross-cutting guides in `docs/guides/`. Open every doc with a short paragraph that says what it covers in plain terms, naming the generic concept alongside any codename (for example, "Azul network upgrade"), so a text search finds it.
- Use `#![doc = include_str!("../README.md")]` for crate documentation in `lib.rs`; never use `//!` comments there.
- Begin every `mod.rs` file with a `//!` module doc comment describing its contents.
- Do not suppress Clippy warnings with `#![allow(missing_docs)]` or other allow-lints. Fix the underlying issue.

## Rust Structure and Style

- Put all `use` imports at the top of the file or at the top of a `mod` block. Do not place imports inside functions or closures.
  - Conditional imports may live inside their `#[cfg(...)]`-gated block, such as a `#[cfg(test)] mod tests` or feature-gated function.
  - Imports inside `macro_rules!` bodies are allowed when the macro needs them in its expansion context.
- Do not destructure a value merely to read its fields, such as `let Self { width, height } = self`. Access fields directly, such as `self.width` and `self.height`.

```rust
// Avoid
fn area(&self) -> u32 {
    let Self { width, height } = self;
    width * height
}

// Prefer
fn area(&self) -> u32 {
    self.width * self.height
}
```

- Prefer simple call chains over indirection. Inline methods that perform only one internal operation, and call an inner type directly instead of adding a forwarding method.

```rust
// Avoid: a wrapper that just forwards to the inner type
impl Wallet {
    fn balance(&self) -> u64 {
        self.account.balance()
    }
}
let bal = wallet.balance();

// Prefer: call the inner type's method directly
let bal = wallet.account.balance();
```

## Tracing

- Use structured tracing with key-value fields for all dynamic data. Keep the message string static.
- Use `%` for `Display` values and `?` for `Debug` values.
- Write `info!(block = %block_number, "processed block")`, not `info!("processed block {block_number}")`.
- Write `error!(error = %error, peer = %peer_id, "connection failed")`, not `error!("connection to {peer_id} failed: {error}")`.

## Testing

- Keep unit tests colocated with their implementation in a `#[cfg(test)] mod tests { ... }` block. Do not create standalone `tests.rs` modules for unit tests.
- Place `#[cfg(test)] mod tests { ... }` at the end of the file, after all non-test code.
- Test observable behavior through public APIs. Do not create tautological or change-detector tests that duplicate production logic or assert incidental implementation details.
  - Tests should fail for behavior regressions and survive behavior-preserving refactors.
  - Interaction assertions are appropriate only when the interaction itself is part of the contract.
- For test doubles of internal traits, default to `#[cfg_attr(test, mockall::automock)]` rather than hand-rolling a fake. See `crates/consensus/service/src/actors/*/client.rs`, `crates/consensus/service/src/actors/network/gossip.rs`, `crates/consensus/service/src/follow/local.rs`, and `crates/consensus/service/src/follow/source.rs`.
- Hand-roll a fake only when `automock` cannot express the required behavior, such as:
  - A trait method returns a non-constructible builder type like Alloy's `ProviderCall` or `EthGetBlock`.
  - The double needs one call log ordered across multiple trait methods.
  - Tests must mutate scripted responses while calls are in flight.
- Document the specific reason for a hand-rolled fake in its module doc comment. Examples live in `crates/consensus/service/src/test_utils/fake_engine_client.rs` and `fake_l1.rs`.

## Recurring Review Expectations

- Keep changes scoped; split unrelated refactors and remove code and dependencies your change makes unused.
- Reuse existing types, APIs, and fixtures; add abstractions or configuration only for concrete supported use cases.
- Prefer domain types and named constants over magic strings and numbers; name values by their meaning and units.
- Keep modules and actor loops focused; put state-specific logic on the type that owns it.
- Document contracts and non-obvious reasons, not code narration or change history; keep docs consistent with behavior.
- Preserve historical fork behavior and parity across execution paths; test both old and new fork behavior.
- Avoid unnecessary I/O, cloning, and allocation on hot paths; benchmark production code when claiming performance gains.
- Make async lifetimes explicit: bound waits, distinguish retryable from terminal errors, and clean up tasks on failure and shutdown.

## Cancellation and Process Lifetimes

Make cancellation explicit when implementing or reviewing asynchronous operations,
tasks, RPCs, queues, child processes, timeouts, retries, or shutdown in any language.
Cancellation is a contract of the whole operation, not just its last await. Apply
this review to the requested change and its producers/consumers, not a repository-wide audit.

### Trace Ownership and Effects

1. Read the operation and its caller. Identify when work is accepted, when effects
   become visible, and when completion is acknowledged. Follow queues, spawned
   tasks, RPCs, and process boundaries to the actual worker and its owner.
2. At each relevant suspension or handoff, trace what survives a losing race/select
   branch, timeout, early return, caller disconnect, task abort, process termination,
   or shutdown. State whether interruption stops execution, requests a cooperative
   stop, transfers ownership, or merely stops waiting.
3. Document who observes completion/errors and releases resources for work that
   can outlive its caller. A token, closed response channel, dropped handle, or kill
   request is not an acknowledged stop. Identify where cancellation is observed;
   verify the library's documented behavior when it is not clear locally.
4. Trace retry and recovery. A new attempt must not unintentionally overlap an old
   one, repeat accepted effects, discard progress, or consume a stale response.
   An unknown outcome is not proof of failure: retain completion, reconcile status,
   or use a justified idempotency mechanism before retrying. Backoff alone does not
   make retries safe.

### Check the Whole Operation

- In repeated races or actor selects, dropping and recreating a losing operation
  must not lose required progress or repeat effects. Otherwise retain the pending
  operation or keep progress in actor-owned state. In Rust, select on `&mut` of a
  future pinned outside the repeated `select!` loop; recreating and pinning it on
  each iteration does not preserve it. Check starvation and responsiveness too.
- Review submission and response handling together. A cancellation-safe receive
  primitive does not make a surrounding send-then-receive operation safe to restart.
  Dropping a response receiver does not retract an enqueued request, including
  one that has executed but has not yet replied.
- Do not infer cancellation from dropping task or process handles. Dropping a
  Tokio `JoinHandle` detaches the task; aborting an async task does not undo effects
  or stop remote work. Started `spawn_blocking` work cannot be aborted. Do not add
  detached tasks just to hide a lost-result bug.
- For child processes, specify termination, bounded waits, reaping, and any
  descendants that must also stop. Aborting a parent need not stop its children
  or blocking work. Verify the API's actual drop behavior.
- Distinguish graceful draining from forced termination. State which accepted work
  may finish, what may be abandoned, and how restart handles incomplete or already
  committed effects. Abrupt process death may bypass cleanup; dropping a local
  future or handle does not imply rollback.
- Intentional abandonment and background work are allowed when ownership and
  effects satisfy the contract. Do not flag every non-cancellation-safe future,
  require cancellation tokens everywhere, or prescribe pinning as a universal fix.

### Exercise the Dangerous Interleaving

- Use existing fixtures and public interfaces to interrupt at applicable boundaries:
  before acceptance, after acceptance while completion is pending, and after an
  effect but before acknowledgement. Use channels/barriers and controlled or paused
  time instead of hoping a sleep reproduces a race.
- Assert completion or acknowledged cancellation, no unintended duplicate effects,
  responsive control requests, and cleanup of owned resources. Retaining a receiver
  alone is not proof of eventual progress. For a bug fix, show the same scenario
  fails without the fix and passes with it.
- Keep regression tests for supported observable contracts, not private helpers or
  incidental call order. In review-only work, report missing coverage without
  modifying files or exercising shared systems.
- For each finding, give the file/line, interruption point, surviving work or lost
  state, failing event sequence, user-visible consequence, and smallest fix.
  Separate confirmed defects from assumptions about traffic or library semantics.
  If no defect is found, state the scope examined and untested paths; static
  inspection and lints do not prove cancellation safety.
