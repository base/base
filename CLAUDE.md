# Agent Instructions

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
- Document the specific reason for a hand-rolled fake in its module doc comment. Examples live in `crates/consensus/service/src/test_utils/fake_engine_client.rs`, `fake_l1.rs`, and `fake_gossip.rs`.
