lib.rs files must be minimal with no logic. Use `#![doc = include_str!("../README.md")]` for the crate doc string, never `//!` comments. Group each module declaration with its re-export (mod foo; pub use foo::Bar;) rather than listing all mods then all pub uses. Modules must not be `pub` or `pub(crate)` unless they are test utilities (e.g. `pub mod test_utils`). All structs, types, enums, and functions within modules should be `pub` and properly re-exported from lib.rs. No private or pub(crate) types. Prefer placing functions as methods on a type (even a unit struct) rather than as bare functions, so the public API exports types, not loose functions.

Do not add `#![allow(missing_docs)]` or other allow-lints to suppress clippy warnings. Fix the underlying issue instead.

Binary crates (bin/) should contain minimal glue code. All meaningful logic belongs in library crates.

Cargo.toml dependencies should be sorted by line length (waterfall style) and logically grouped as done in the rest of the workspace. Features sections go at the bottom of the manifest. All crate and binary Cargo.toml files must inherit lints from the workspace with `[lints] workspace = true`.
