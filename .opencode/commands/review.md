---
description: Review staged changes before committing (Base Reth Node)
---

You are reviewing staged changes for Base Reth Node — a Rust Ethereum L2 node built on Reth.

## Staged Changes

**Branch:** !`git branch --show-current`

**Files changed:**
!`git diff --cached --stat`

**Full diff:**
!`git diff --cached`

If there are no staged changes, stop and tell the user to stage files first with `git add`.

## Codebase Context

- 9 crate groups: crates/{core, execution, consensus, builder, batcher, proof, utilities, infra, txpool}
- 14 binaries in bin/
- devnet/: system and E2E test infrastructure
- Error handling: thiserror enums with #[from] for automatic From impls
- Async: tokio (select!, mpsc, watch, oneshot), async-trait
- Workspace lints: clippy (52 rules, -D warnings), rustfmt (2024 edition), cargo-deny, cargo-udeps
- Structured tracing: key=value fields, never interpolated strings
- All crates use `base-` name prefix

## Review Guidelines

- Review like a senior Rust engineer on the team
- Focus on correctness, safety, and idiomatic Rust
- DO NOT comment on formatting or style — clippy and rustfmt handle that
- Be direct. No praise or filler. If everything looks fine, say "Ready to commit" and suggest a commit message

## What to Look For

- **Error handling**: .unwrap()/.expect() in non-test code, discarded error context (.map_err(|_| ...)), missing From impls
- **Memory & performance**: unnecessary .clone() on large types in hot paths, unbounded collections from external input
- **Concurrency**: locks held across .await, channel misuse (no backpressure, unhandled closed channels), cancellation safety in tokio::select!
- **Safety**: unsafe without // SAFETY comments, unchecked arithmetic on financial/gas values, HashMap in determinism-sensitive code
- **API design**: missing #[must_use] on Result-returning public methods, breaking changes to public interfaces
- **Architecture**: dependency direction violations (shared/core depending on client/builder), tight coupling between crate layers
- **Rust idioms**: &String instead of &str, &Vec<T> instead of &[T], manual implementations of standard traits, needless lifetime annotations

## Workspace Conventions (from AGENTS.md)

- lib.rs: minimal, no logic, use `#![doc = include_str!("../README.md")]`, group mod+re-export together
- Modules: not pub/pub(crate) unless test utilities; all types within modules are pub
- mod.rs: must begin with `//!` doc comment
- Cargo.toml: dependencies sorted by line length (waterfall), features at bottom, `[lints] workspace = true`
- No features in workspace root Cargo.toml
- Structured tracing: `info!(block = %block_number, "processed block")` not `info!("processed block {block_number}")`
- Tests: `#[cfg(test)] mod tests` at end of file
- Imports: `use` at top of file/mod block, never inside function bodies
- No `#![allow(missing_docs)]` or other allow-lints — fix the underlying issue

## Output Format

Start with one of:
- **Ready to commit** — no issues found. Suggest a commit message.
- **Issues found** — list each issue with file, line, and suggested fix. Then state whether the issues are blocking or advisory.
