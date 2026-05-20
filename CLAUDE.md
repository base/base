# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

### Setup (one-time)
```sh
just setup        # installs fast linker (lld/mold) and builds test contracts
```

### Build
```sh
just build::release       # full workspace, release profile
just build::all-targets   # debug, all targets (runs contract + ELF builds first)
just build::node          # only the base-reth-node binary
```
On macOS, set `RISC0_SKIP_BUILD_KERNELS=1` to skip Metal kernel compilation for `check`/`clippy` (handled automatically by `just`).

### Test
```sh
just test                         # all workspace tests, all features (uses cargo-nextest)
just test-affected                # only crates affected by changes vs main
cargo nextest run -p <crate-name> # single crate
cargo nextest run -p <crate-name> <test-name>  # single test
```

### Lint / Format
```sh
just check::clippy    # clippy, warnings as errors
just check::format    # rustfmt check (nightly)
just check::udeps     # unused dependency check
just check::deny      # license and ban checks
just fix              # auto-fix formatting + clippy issues
just ci               # full CI suite (fix + all checks + tests + lychee + zepter)
just pr               # PR-scoped CI (affected crates only for tests)
```

### Benchmarks
```sh
just bench-flashblocks   # flashblocks pending state benchmarks
just bench-proof-mpt     # MPT trie node benchmarks
```

### Specs site (local)
```sh
just specs    # runs docs/specs with bun (requires Node 22+)
```

---

## Architecture

### Repository Layout

```
bin/          Binary entry points — thin glue only; all logic lives in crates/
crates/
  consensus/  Rollup consensus node (derivation, p2p gossip, engine, providers)
  execution/  Reth-based execution node (EVM, txpool, flashblocks, RPC, payload)
  builder/    Flashblocks block builder
  batcher/    L2 batch data submission to L1 DA layer
  proof/      Fault proof infrastructure (ZK, TEE/Nitro, MPT)
  infra/      Infrastructure services (ingress-rpc, basectl, websocket-proxy, etc.)
  common/     Shared domain types (chain specs, genesis, rpc-types, signer, etc.)
  utilities/  Dev utilities (test-utils, runtime, health, jwt, metrics, etc.)
```

### Key Binaries

| Binary | Role |
|---|---|
| `bin/node` | Reth-based execution node with flashblocks and metering |
| `bin/consensus` | Rollup consensus node (derivation + p2p + engine) |
| `bin/builder` | Flashblocks block builder |
| `bin/batcher` | L1 DA batch submitter |
| `bin/proposer` | TEE-based output proposer |
| `bin/challenger` | ZK dispute game challenger |
| `bin/mempool-rebroadcaster` | Bridges Geth↔Reth mempools |
| `bin/ingress-rpc` | JSON-RPC ingress backed by Kafka |
| `bin/prover/` | ZK prover (SP1/Succinct) and TEE prover (Nitro) |

### Consensus Node — Actor Model

`crates/consensus/service` is the composition layer only; it owns no domain logic. Each subsystem (derivation, engine, gossip, discovery, peers, providers, safedb) is an independent async actor implementing the `NodeActor` trait (`async fn start(self, ctx) -> Result<(), Error>`). Actors share no mutable state — all cross-actor communication uses typed channels (`mpsc` for requests, `watch` for broadcast state). A root `CancellationToken` propagates shutdown: any actor error cancels the token and drains all remaining actors. Two top-level service variants exist: `RollupNode` (full, with block production) and `FollowNode` (follow-only).

### Flashblocks

The builder produces block chunks (flashblocks) at sub-second intervals and publishes them via WebSocket before merging into full blocks. `crates/execution/flashblocks` subscribes to these, maintaining `FlashblocksState` which reconciles the pending blocks with canonical chain updates (`CanonicalBlockReconciler`, `ReorgDetector`). This drives pending-state-aware RPC extensions (`eth_call`, `eth_getBalance`, etc.) that serve results before finalization.

### Proof System

Three backends under `crates/proof/`:

- **ZK** (`crates/proof/zk/`) — SP1/Succinct validity proofs; includes driver, client, on-chain contracts, and outbox.
- **TEE/Nitro** (`crates/proof/tee/`) — AWS Nitro enclave attestation proofs with a registrar for automated signer registration.
- **MPT** (`crates/proof/mpt/`) — Merkle Patricia Trie node proofs.

The proof host reads L1/L2 data via a preimage oracle (`CachingOracle`); `BaseExecutor` drives the state transition. `crates/proof/proposer` and `crates/proof/challenge` wire proofs into the dispute game protocol on-chain.

---

## Code Style

`lib.rs` files must be minimal with no logic. Use `#![doc = include_str!("../README.md")]` for the crate doc string, never `//!` comments. Group each module declaration with its re-export (`mod foo; pub use foo::Bar;`) rather than listing all mods then all pub uses. Modules must not be `pub` or `pub(crate)` unless they are test utilities (e.g. `pub mod test_utils`). All structs, types, enums, and functions within modules should be `pub` and properly re-exported from `lib.rs`. No private or `pub(crate)` types. Prefer placing functions as methods on a type (even a unit struct) rather than as bare functions, so the public API exports types, not loose functions.

Do not add `#![allow(missing_docs)]` or other allow-lints to suppress clippy warnings. Fix the underlying issue instead.

Binary crates (`bin/`) should contain minimal glue code. All meaningful logic belongs in library crates.

`Cargo.toml` dependencies should be sorted by line length (waterfall style) and logically grouped as done in the rest of the workspace. Features sections go at the bottom of the manifest. All crate and binary `Cargo.toml` files must inherit lints from the workspace with `[lints] workspace = true`.

Do not add features to dependencies in the workspace root `Cargo.toml`. Features must be enabled only by the individual crates or binaries that need them, to prevent feature leakage into `no_std` crates.

All crates in the workspace should have a `base-` prefix in their crate name (e.g. `base-enclave`, `base-builder-core`).

Every `mod.rs` file must begin with a `//!` module doc comment describing what the module contains.

All `use` imports must be at the top of the file or the top of a `mod` block. Never place `use` statements inside function bodies or closures. Exception: conditional imports behind `#[cfg(...)]` may be scoped to the `cfg`-gated block (e.g., inside a `#[cfg(test)] mod tests`, `#[cfg(feature = "...")]` function, or similar) rather than hoisted to the top of the file. Another exception: `use` inside `macro_rules!` bodies is acceptable when the macro needs to import items in its expansion context.

Use structured tracing instead of interpolated strings. Always use key=value fields for any dynamic data: `info!(block = %block_number, "processed block")` rather than `info!("processed block {block_number}")`. Use `%` for Display, `?` for Debug. The message string should be a static description; all variable data goes in fields. Correct: `error!(error = %e, peer = %peer_id, "connection failed")`. Incorrect: `error!("connection to {peer_id} failed: {e}")`.

`#[cfg(test)] mod tests { ... }` must always be placed at the end of the file, after all non-test code.
