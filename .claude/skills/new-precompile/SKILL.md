---
name: new-precompile
description: "Guide for adding a new native precompile. Use when creating a new precompile domain or adding a precompile to an existing domain. Triggers on: new precompile, add precompile, create precompile, native precompile."
---

# New Native Precompile

## Step 1 — Do you need a new domain or add to an existing one?

A **domain** is a folder inside `crates/common/precompiles/src/` containing one or more precompiles that belong together.

| Signal | Decision |
|---|---|
| Shares storage slots or factory initialization with an existing precompile | Add to existing domain |
| Needs to call into an existing precompile's address space | Add to existing domain |
| Completely orthogonal — no shared storage, no factory coupling | New domain |
| Unsure | New domain — merging later is cheaper than untangling coupling |

**Existing domains** — check `crates/common/precompiles/src/` for domain folders (exclude infrastructure crates `precompile-macros` and `precompile-storage`).

---

## Step 2a — Adding a precompile to an existing domain

Inside the domain folder (`crates/common/precompiles/src/<domain>/`), add:

```
<domain>/
  abi/
    <name>.rs           ← sol! interface for the new precompile
  <name>/
    mod.rs
    storage.rs          ← #[contract] struct (storage layout)
    dispatch.rs         ← ABI dispatch
    evm.rs              ← EVM entry point struct
```

Re-export from `<domain>/abi/mod.rs` and `<domain>/mod.rs`. If logic is shared with other precompiles in the domain, put it in `<domain>/shared/`.

---

## Step 2b — Creating a new domain

```
crates/common/precompiles/src/<domain>/
  mod.rs
  abi/
    mod.rs              ← re-exports all sol! types in this domain
    <name>.rs           ← sol! interface per precompile
  shared/               ← logic shared across precompiles in this domain (add when needed)
  <name>/
    mod.rs
    storage.rs          ← #[contract] struct
    dispatch.rs
    evm.rs              ← EVM entry point struct
```

### Register the new domain module

In `crates/common/precompiles/src/lib.rs`, declare the new module:

```rust
mod <domain>;
```

### Update `crates/common/precompiles/Cargo.toml`

If this is the first domain using the storage/ABI infrastructure, add the missing dependencies:

```toml
[dependencies]
alloy-sol-types = { workspace = true, features = ["std"] }
base-precompile-macros  = { path = "../precompile-macros" }
base-precompile-storage = { path = "../precompile-storage" }
```

---

### `<domain>/abi/<name>.rs`

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

### `<domain>/<name>/storage.rs`

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

### `<domain>/<name>/dispatch.rs`

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

use super::super::abi::I<Name>;
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

### `<domain>/<name>/evm.rs`

The EVM entry point struct lives in the same domain folder so that all wiring stays inside `base-common-precompiles`.

> **Note:** `StorageCtx::enter` requires `S: Sized` and cannot be called directly with
> `&mut dyn PrecompileStorageProvider`. The `EvmPrecompileStorageProvider` is `Sized`, so
> it is created here before passing into the closure.

```rust
use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::{Address, Bytes, address};
use base_precompile_storage::{EvmPrecompileStorageProvider, StorageCtx};
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};

use super::{<Name>, dispatch};

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
- `StorageCtx::enter` sets the thread-local that `#[contract]`-generated storage types read from.

### `<domain>/<name>/mod.rs`

```rust
use alloy_primitives::Address;
use base_precompile_storage::{NativePrecompile, PrecompileStorageProvider};
use revm::precompile::PrecompileResult;

pub use dispatch::dispatch;
pub use evm::{ADDRESS, <Name>Precompile};
pub use storage::{<Name>, <NAME>_ADDRESS};

mod dispatch;
mod evm;
mod storage;

impl NativePrecompile for <Name> {
    const ADDRESS: Address = <NAME>_ADDRESS;

    fn execute(_storage: &mut dyn PrecompileStorageProvider) -> PrecompileResult {
        // TODO: wire calldata once PrecompileStorageProvider exposes it
        todo!()
    }
}
```

### `<domain>/mod.rs`

Re-export all public types including `dispatch` so nothing is `unreachable_pub`:

```rust
pub mod abi;
pub mod <name>;

pub use <name>::{ADDRESS, <Name>, <Name>Precompile, <NAME>_ADDRESS, dispatch};
```

---

## Registration

Wiring a new domain precompile into the live EVM requires **two concrete edits**, both inside `crates/common/precompiles/`. The `base-common-evm` crate needs no changes — it already calls `BasePrecompileInstaller::install()` which delegates to `install_into`.

---

### Step R1 — Export the domain from `lib.rs`

**File:** `crates/common/precompiles/src/lib.rs`

Change `mod <domain>;` to `pub mod <domain>;` so callers of the crate can reach the entry point:

```rust
pub mod <domain>;
```

---

### Step R2 — Register the precompile in the installer

**File:** `crates/common/precompiles/src/installer.rs`

Remove the `const` qualifier (dynamic insertion requires `&mut`) and add the fork-gated registration inside `install_into`:

```rust
pub fn install_into(self, precompiles: &mut PrecompilesMap) {
    if self.spec.upgrade() >= BaseUpgrade::<Fork> {
        precompiles.insert(
            crate::<domain>::ADDRESS,
            crate::<domain>::<Name>Precompile::precompile(),
        );
    }
}
```

> Multiple precompiles at the **same fork** — add additional `insert` calls inside the same `if` block.
> Each fork gets its own `if self.spec.upgrade() >= BaseUpgrade::<Fork>` guard.

---

### Checklist

```
[ ] crates/common/precompiles/Cargo.toml          storage/macros deps added (first domain only)
[ ] crates/common/precompiles/src/<domain>/        folder created with all files
[ ] crates/common/precompiles/src/lib.rs           pub mod <domain>; added
[ ] crates/common/precompiles/src/installer.rs     install_into wired with fork guard
[ ] cargo check -p base-common-precompiles         compiles clean
[ ] cargo test  -p base-common-precompiles         all tests pass
[ ] cargo check -p base-common-evm                 still compiles (smoke check)
```

---

## Slot rules (brief)

- Slots are append-only — **never reorder or reuse across hardforks**
- `#[slot(N)]` pins to absolute slot N
- Mapping slot: `keccak256(lpad32(key) ‖ slot_be32)`
