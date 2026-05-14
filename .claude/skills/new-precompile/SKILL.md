---
name: new-precompile
description: "Guide for adding a new native precompile. Use when creating a new precompile domain or adding a precompile to an existing domain. Triggers on: new precompile, add precompile, create precompile, native precompile."
---

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

Wiring a domain precompile into the live EVM requires **four concrete edits** across two crates.
The domain crate (`base-precompile-<domain>`) never imports from `base-common-evm`; the dependency
only flows the other way.

---

### Step R1 — Create the EVM entry point

**New file:** `crates/common/evm/src/precompiles/<name>/mod.rs`

```rust
//! EVM entry point for the <Name> native precompile.

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::{Address, Bytes, address};
use base_precompile_<domain>::{<Name>, dispatch};
use base_precompile_storage::{EvmPrecompileStorageProvider, StorageCtx};
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};

/// Canonical address of the <Name> precompile.
pub const ADDRESS: Address = address!("<20-byte-hex>");

/// EVM entry point for the <Name> precompile.
#[derive(Debug, Default, Clone, Copy)]
pub struct <Name>Precompile;

impl <Name>Precompile {
    /// Returns a [`DynPrecompile`] registerable with [`PrecompilesMap`].
    pub fn precompile() -> DynPrecompile {
        DynPrecompile::new_stateful(PrecompileId::Custom("<Name>".into()), Self::run)
    }

    fn run(input: PrecompileInput<'_>) -> PrecompileResult {
        if !input.is_direct_call() {
            return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
        }
        // Capture calldata before consuming input into the provider.
        let calldata: Bytes = input.data.to_vec().into();
        let mut provider = EvmPrecompileStorageProvider::new(input);
        StorageCtx::enter(&mut provider, || {
            let mut pc = <Name>::new();
            dispatch(&mut pc, &calldata)
        })
    }
}
```

Key points:
- `is_direct_call()` guard rejects DELEGATECALL/CALLCODE — always include it.
- Calldata is cloned **before** `input` is consumed by `EvmPrecompileStorageProvider::new`.
- `StorageCtx::enter` works here because `EvmPrecompileStorageProvider` is `Sized`; it sets the
  thread-local that `#[contract]`-generated storage types read from.

---

### Step R2 — Expose the sub-module

**File:** `crates/common/evm/src/precompiles/mod.rs`

Add one line:

```rust
pub mod <name>;
```

---

### Step R3 — Register it fork-gated in the factory

**File:** `crates/common/evm/src/factory.rs`

Import the entry point at the top:

```rust
use crate::precompiles::<name>::<Name>Precompile;
```

Inside `BaseEvmFactory::precompiles`, add the address inside the correct fork guard.
If the fork already has a `set_precompile_lookup` block, add an `else if` branch to it.
If this is the first precompile at a new fork, add a new `if` block:

```rust
if spec.is_enabled_in(BaseUpgrade::<Fork>) {
    precompiles.set_precompile_lookup(|address: &alloy_primitives::Address| {
        if *address == crate::precompiles::<name>::ADDRESS {
            Some(<Name>Precompile::precompile())
        } else {
            None
        }
    });
}
```

> Multiple precompiles at the **same fork** share one `set_precompile_lookup` call — use
> chained `if / else if` inside a single block. Each fork gets its own `if spec.is_enabled_in`
> block.

---

### Step R4 — Add the domain crate as a dependency of `base-common-evm`

**File:** `crates/common/evm/Cargo.toml`

```toml
base-precompile-<domain> = { path = "../precompile-<domain>" }
```

---

### Checklist

```
[ ] crates/common/evm/src/precompiles/<name>/mod.rs   created
[ ] crates/common/evm/src/precompiles/mod.rs          pub mod <name>; added
[ ] crates/common/evm/src/factory.rs                  address in fork guard + import
[ ] crates/common/evm/Cargo.toml                      domain crate dep added
[ ] cargo check -p base-common-evm                    compiles clean
[ ] cargo test  -p base-common-evm                    all tests pass
```

## Slot rules (brief)

- Slots are append-only — **never reorder or reuse across hardforks**
- `#[slot(N)]` pins to absolute slot N
- Mapping slot: `keccak256(lpad32(key) ‖ slot_be32)`
