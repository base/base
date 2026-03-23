# `base-revm`

Base-specific constants, types, and helpers for the revm EVM implementation.

## Overview

Integrates revm with Base, providing Base-specific EVM execution infrastructure. Includes
`OpEvm` and associated builder/handler types, `OpTransaction` and `OpTransactionBuilder` for
transaction handling, `BasePrecompiles` with hardfork-accelerated precompile sets (BLS, BN254,
Fjord, Granite, Isthmus, Jovian), `L1BlockInfo` for fee calculation, and `OpSpecId` variants
mapping each hardfork to a revm spec.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-revm = { workspace = true }
```

```rust,ignore
use base_revm::{OpBuilder, BasePrecompiles};

let evm = OpBuilder::new(db)
    .with_precompiles(BasePrecompiles::jovian())
    .build();
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
