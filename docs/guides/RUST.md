# Rust Development for Agents

This guide contains the repository's Rust development and review rules for agents,
including crate APIs, Cargo features, documentation, tracing, tests, and explicit
cancellation of asynchronous work. Read it before changing or reviewing Rust code
or Cargo manifests. See [TESTING.md](TESTING.md) for verification commands.

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

## Make Cancellation Explicit

Use the [reviewing-cancellation skill](../../.agents/skills/reviewing-cancellation/SKILL.md)
when implementing or reviewing asynchronous lifetimes, timeouts, retries, or shutdown.
Cancellation is a contract of the whole operation, not just of its last `.await`.

- For an operation that can outlive its caller, document who owns completion and
  what dropping, timing out, or aborting the caller actually does: stop the work,
  request cooperative cancellation, or only stop waiting. State what happens to
  partial effects and whether retrying can duplicate work. Intentional background
  work must have an owner for its result, errors, and cleanup.
- Trace every exit from a pending operation: a losing `select!` branch, timeout,
  task abort, early return, channel closure, and shutdown. A cancellation token is
  a request, not proof that work stopped; identify where it is observed and where
  completion is acknowledged.
- In repeated `select!` calls, dropping and recreating a branch must not lose
  required progress or repeat effects. Otherwise retain the future across loop
  iterations and select on `&mut` of the pinned future, or keep progress in
  actor-owned state. Pinning inside the repeated select loop does not preserve
  ownership across iterations. Intentional abandonment, such as shutdown, still
  needs an explicit policy for outstanding work.
- Inspect request submission and response handling together. A cancellation-safe
  channel receive does not make a surrounding send-then-receive operation safe to
  restart. Dropping the response receiver does not retract an enqueued request.
- Do not infer cancellation from dropping a task or process handle. Dropping a
  Tokio `JoinHandle` detaches the task; aborting an async task does not undo its
  effects or stop remote work. Started `spawn_blocking` work cannot be aborted.
  For child processes, specify termination, waiting/reaping, and any descendants
  that must also stop; verify the API's actual drop behavior.
- A timeout after submission can leave an unknown outcome. Reconcile status,
  retain the original completion, or use a documented idempotency mechanism
  before retrying. Backoff alone does not make retries safe.
- Test interruption at meaningful boundaries: before submission, after acceptance
  but before completion, and after an effect but before acknowledgement, as
  applicable. Use barriers/channels and paused time rather than timing races.
  Assert completion or acknowledged cancellation, no unintended duplicate
  effects, continued control-plane responsiveness, and required cleanup. For a
  bug fix, demonstrate that the regression fails before the fix and passes after.

See [Tokio's cancellation-safety guidance](https://docs.rs/tokio/latest/tokio/macro.select.html#cancellation-safety).
Compiler and lint checks do not establish this cross-task contract; review the
ownership and effect boundaries and test the interleaving that could violate it.
