# `base-execution-primitives`

Primitive types for Base execution.

## Overview

Defines the core Base primitive types used throughout the Base execution layer: `OpPrimitives`
(the `NodePrimitives` implementation), `OpBlock`, `OpBlockBody`, `OpReceipt`,
`OpTransactionSigned`, and the `DepositReceipt` trait. These are the canonical types passed
between execution subsystems (engine, EVM, storage, payload builder).

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-execution-primitives = { workspace = true }
```

```rust,ignore
use base_execution_primitives::{OpPrimitives, OpBlock, OpReceipt};

fn process<N: NodePrimitives<Block = OpBlock, Receipt = OpReceipt>>() { ... }
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
