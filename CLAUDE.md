lib.rs files must be minimal with no logic. Use `#![doc = include_str!("../README.md")]` for the crate doc string, never `//!` comments. Group each module declaration with its re-export (mod foo; pub use foo::Bar;) rather than listing all mods then all pub uses. Modules must not be `pub` or `pub(crate)` unless they are test utilities (e.g. `pub mod test_utils`). All structs, types, enums, and functions within modules should be `pub` and properly re-exported from lib.rs. No private or pub(crate) types. Prefer placing functions as methods on a type (even a unit struct) rather than as bare functions, so the public API exports types, not loose functions.

Do not add `#![allow(missing_docs)]` or other allow-lints to suppress clippy warnings. Fix the underlying issue instead.

Binary crates (bin/) should contain minimal glue code. All meaningful logic belongs in library crates.

Cargo.toml dependencies should be sorted by line length (waterfall style) and logically grouped as done in the rest of the workspace. Features sections go at the bottom of the manifest. All crate and binary Cargo.toml files must inherit lints from the workspace with `[lints] workspace = true`.

Do not add features to dependencies in the workspace root Cargo.toml. Features must be enabled only by the individual crates or binaries that need them, to prevent feature leakage into no_std crates.

All crates in the workspace should have a `base-` prefix in their crate name (e.g. `base-enclave`, `base-builder-core`).

All `use` imports must be at the top of the file or the top of a `mod` block. Never place `use` statements inside function bodies or closures. Exception: conditional imports behind `#[cfg(...)]` may be scoped to the `cfg`-gated block (e.g., inside a `#[cfg(test)] mod tests`, `#[cfg(feature = "...")]` function, or similar) rather than hoisted to the top of the file. Another exception: `use` inside `macro_rules!` bodies is acceptable when the macro needs to import items in its expansion context.

Use structured tracing instead of interpolated strings. Always use key=value fields for any dynamic data: `info!(block = %block_number, "processed block")` rather than `info!("processed block {block_number}")`. Use `%` for Display, `?` for Debug. The message string should be a static description; all variable data goes in fields. Correct: `error!(error = %e, peer = %peer_id, "connection failed")`. Incorrect: `error!("connection to {peer_id} failed: {e}")`.

