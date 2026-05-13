# New Native Precompile

## Step 1 — Do you need a new domain or add to an existing one?

A **domain** is a crate containing one or more precompiles that belong together.

| Signal | Decision |
|---|---|
| Shares storage slots or factory initialization with an existing precompile | Add to existing domain |
| Needs to call into an existing precompile's address space | Add to existing domain |
| Completely orthogonal — no shared storage, no factory coupling | New domain |
| Unsure | New domain — merging later is cheaper than untangling coupling |

**Existing domains:**
```
crates/common/
  precompile-macros/    ← infrastructure (not a domain)
  precompile-storage/   ← infrastructure (not a domain)
  precompile-tokens/    ← token domain (regular, stablecoin, security, factories)
  precompile-oracle/    ← oracle domain
```

---

## Step 2a — Adding a precompile to an existing domain

Inside the domain crate, add:

```
src/
  abi/
    <name>.rs           ← sol! interface for the new precompile
  <name>/
    mod.rs
    storage.rs          ← #[contract] struct (storage layout)
    dispatch.rs         ← ABI dispatch
```

Re-export from `abi/mod.rs` and `lib.rs`.

If logic is shared with other precompiles in the domain, put it in `shared/`.

---

## Step 2b — Creating a new domain

```
crates/common/precompile-<domain>/
  Cargo.toml
  src/
    lib.rs
    abi/
      mod.rs            ← re-exports all sol! types in this domain
      <name>.rs         ← sol! interface per precompile
    shared/             ← logic shared across precompiles in this domain (add when needed)
    <name>/
      mod.rs
      storage.rs        ← #[contract] struct
      dispatch.rs
```

### `Cargo.toml`

```toml
[package]
name = "base-precompile-<domain>"
description = "<Description>"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
homepage.workspace = true
repository.workspace = true
exclude.workspace = true

[lints]
workspace = true

[dependencies]
alloy-primitives.workspace = true
alloy-sol-types = { workspace = true, features = ["std"] }
base-precompile-storage = { path = "../precompile-storage" }
base-precompile-macros  = { path = "../precompile-macros" }
revm.workspace = true
thiserror.workspace = true
```

### `src/abi/<name>.rs`

```rust
use alloy_sol_types::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface I<Name> {
        // function signatures
        // events
        // errors
    }
}
```

### `src/<name>/storage.rs`

```rust
use alloy_primitives::{Address, address};
use base_precompile_macros::contract;

pub const <NAME>_ADDRESS: Address = address!("0x...");

// Slots are append-only — never reorder across hardforks
#[contract(addr = <NAME>_ADDRESS)]
pub struct <Name> {
    // pub field: Type,   // slot 0
}
```

### `src/<name>/mod.rs`

```rust
use base_precompile_storage::{
    NativePrecompile, PrecompileStorageProvider, StorageCtx,
};
use revm::precompile::PrecompileResult;

pub use storage::<Name>, storage::<NAME>_ADDRESS;
mod storage;

impl NativePrecompile for <Name> {
    const ADDRESS: Address = <NAME>_ADDRESS;

    fn execute(storage: &mut dyn PrecompileStorageProvider) -> PrecompileResult {
        StorageCtx::enter(storage, || {
            let mut pc = <Name>::new();
            let ctx = StorageCtx;
            // TODO: decode calldata and dispatch
            todo!()
        })
    }
}
```

### `src/lib.rs`

```rust
#![doc = include_str!("../README.md")]

pub mod abi;
pub mod <name>;
```

## Registration

One line in the precompile registry:
```rust
// BasePrecompiles::register::<Name>();
```

## Slot rules (brief)

- Slots are append-only — **never reorder or reuse across hardforks**
- `#[slot(N)]` pins to absolute slot N
- Mapping slot: `keccak256(lpad32(key) ‖ slot_be32)`
