# New Native Precompile

## Step 1 — Do you need a new domain or add to an existing one?

A **domain** is a crate containing one or more precompiles that belong together.

| Signal | Decision |
|---|---|
| Shares storage slots or factory initialization with an existing precompile | Add to existing domain |
| Needs to call into an existing precompile's address space | Add to existing domain |
| Completely orthogonal — no shared storage, no factory coupling | New domain |
| Unsure | New domain — merging later is cheaper than untangling coupling |

**Existing domains** — check `crates/common/` for `precompile-*` crates that are not `precompile-macros` or `precompile-storage` (those are infrastructure, not domains).

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

Re-export from `abi/mod.rs` and `lib.rs`. If logic is shared with other precompiles in the domain, put it in `shared/`.

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
revm.workspace = true
base-precompile-macros  = { path = "../precompile-macros" }
base-precompile-storage = { path = "../precompile-storage" }

[features]
test-utils = []   # required: #[contract] uses #[cfg(feature = "test-utils")] internally
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

### `src/<name>/dispatch.rs`

`sol! { interface I<Name> { ... } }` generates a **module** named `I<Name>`, not an enum.
The dispatch enum is `I<Name>::I<Name>Calls`. Three traits must be in scope:

- `Handler` — for `.read()` / `.write()` on `Slot<T>` fields
- `SolInterface` — for `I<Name>::I<Name>Calls::abi_decode`
- `SolCall` — for `abi_encode_returns` on functions with return values

```rust
use alloy_primitives::Bytes;
use alloy_sol_types::{SolCall, SolInterface};
use base_precompile_storage::{BasePrecompileError, Handler, IntoPrecompileResult, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::abi::I<Name>;
use super::<Name>;

pub fn dispatch(pc: &mut <Name>, calldata: &[u8]) -> PrecompileResult {
    let ctx = StorageCtx;
    inner(pc, calldata).into_precompile_result(ctx.gas_used(), |b| b)
}

fn inner(pc: &mut <Name>, calldata: &[u8]) -> base_precompile_storage::Result<Bytes> {
    if calldata.len() < 4 {
        return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
    }
    let selector: [u8; 4] = calldata[..4].try_into().unwrap();

    match I<Name>::I<Name>Calls::abi_decode(calldata) {
        Ok(I<Name>::I<Name>Calls::myVoidFn(_)) => {
            // no return value
            Ok(Bytes::new())
        }
        Ok(I<Name>::I<Name>Calls::myGetterFn(_)) => {
            let val = pc.field.read()?;
            // single return: pass value directly, not as a tuple
            Ok(I<Name>::myGetterFnCall::abi_encode_returns(&val).into())
        }
        Err(_) => Err(BasePrecompileError::UnknownFunctionSelector(selector)),
    }
}
```

### `src/<name>/mod.rs`

> **Note:** `StorageCtx::enter` requires `S: Sized` and cannot be called directly with
> `&mut dyn PrecompileStorageProvider`. Leave `execute` as `todo!()` until calldata is
> wired into `PrecompileStorageProvider`.

```rust
use alloy_primitives::Address;
use base_precompile_storage::{NativePrecompile, PrecompileStorageProvider};
use revm::precompile::PrecompileResult;

pub use dispatch::dispatch;
pub use storage::{<Name>, <NAME>_ADDRESS};

mod dispatch;
mod storage;

impl NativePrecompile for <Name> {
    const ADDRESS: Address = <NAME>_ADDRESS;

    fn execute(_storage: &mut dyn PrecompileStorageProvider) -> PrecompileResult {
        // TODO: wire calldata once PrecompileStorageProvider exposes it
        todo!()
    }
}
```

### `src/lib.rs`

Re-export all public types including `dispatch` so nothing is `unreachable_pub`:

```rust
#![doc = include_str!("../README.md")]

pub mod abi;
pub mod <name>;

pub use <name>::{<Name>, <NAME>_ADDRESS, dispatch};
```

## Registration

```rust
// BasePrecompiles::register::<Name>();
```

## Slot rules (brief)

- Slots are append-only — **never reorder or reuse across hardforks**
- `#[slot(N)]` pins to absolute slot N
- Mapping slot: `keccak256(lpad32(key) ‖ slot_be32)`
