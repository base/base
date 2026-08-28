lib.rs files must be minimal with no logic. Use `#![doc = include_str!("../README.md")]` for the crate doc string, never `//!` comments. Group each module declaration with its re-export (mod foo; pub use foo::Bar;) rather than listing all mods then all pub uses. Modules must not be `pub` or `pub(crate)` unless they are test utilities (e.g. `pub mod test_utils`). All structs, types, enums, and functions within modules should be `pub` and properly re-exported from lib.rs. No private or pub(crate) types. Prefer placing functions as methods on a type (even a unit struct) rather than as bare functions, so the public API exports types, not loose functions.

Do not add `#![allow(missing_docs)]` or other allow-lints to suppress clippy warnings. Fix the underlying issue instead.

Binary crates (bin/) should contain minimal glue code. All meaningful logic belongs in library crates.

Cargo.toml dependencies should be sorted by line length (waterfall style) and logically grouped as done in the rest of the workspace. Features sections go at the bottom of the manifest. All crate and binary Cargo.toml files must inherit lints from the workspace with `[lints] workspace = true`.

Do not add features to dependencies in the workspace root Cargo.toml. Features must be enabled only by the individual crates or binaries that need them, to prevent feature leakage into no_std crates.

All crates in the workspace should have a `base-` prefix in their crate name (e.g. `base-enclave`, `base-builder-core`).

Every `mod.rs` file must begin with a `//!` module doc comment describing what the module contains.

All `use` imports must be at the top of the file or the top of a `mod` block. Never place `use` statements inside function bodies or closures. Exception: conditional imports behind `#[cfg(...)]` may be scoped to the `cfg`-gated block (e.g., inside a `#[cfg(test)] mod tests`, `#[cfg(feature = "...")]` function, or similar) rather than hoisted to the top of the file. Another exception: `use` inside `macro_rules!` bodies is acceptable when the macro needs to import items in its expansion context.

Use structured tracing instead of interpolated strings. Always use key=value fields for any dynamic data: `info!(block = %block_number, "processed block")` rather than `info!("processed block {block_number}")`. Use `%` for Display, `?` for Debug. The message string should be a static description; all variable data goes in fields. Correct: `error!(error = %e, peer = %peer_id, "connection failed")`. Incorrect: `error!("connection to {peer_id} failed: {e}")`.

Keep unit tests colocated with their implementation. Do not introduce standalone `tests.rs` modules for unit tests; define tests in the same `.rs` implementation file/module inside a `#[cfg(test)] mod tests { ... }` block.

Do not create tautological tests. Tests that merely restate the implementation without independently validating behavior are considered bad tests.

`#[cfg(test)] mod tests { ... }` must always be placed at the end of the file, after all non-test code.

Do not destructure a type into its fields with `let Self { .. } = self` (or similar `let T { .. } = value`) just to read fields. Access fields directly via `self.field` for readability.

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

Prefer simplifying call chains over adding indirection. If a method only performs a single internal operation, inline that logic at the call site rather than introducing the method. If a method merely forwards to another type, call that type's method directly instead of wrapping it in a new method.

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

For test doubles of internal traits, default to `#[cfg_attr(test, mockall::automock)]` (see `crates/consensus/service/src/actors/*/client.rs`, `crates/consensus/service/src/actors/network/gossip.rs`, `crates/consensus/service/src/follow/local.rs`, `crates/consensus/service/src/follow/source.rs` for examples) rather than hand-rolling a fake. Only hand-roll a fake (as in `crates/consensus/service/src/test_utils/fake_engine_client.rs`, `fake_l1.rs`, `fake_gossip.rs`) when `automock` cannot express the required behavior — e.g. the trait method returns a non-constructible builder type (such as alloy's `ProviderCall`/`EthGetBlock`), the double needs a single call log ordered across multiple trait methods, or scripted responses must be mutated by the test while calls are in flight. Document the specific reason a hand-rolled fake was chosen in the module doc comment.
