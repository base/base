# `base-common-evm`

EVM implementation.

## Overview

Provides Base-specific EVM execution support. Maps hardfork activation timestamps to revm
`SpecId` values, and exposes `BaseEvm`, `BaseEvmFactory`, `BaseBlockExecutor`, and
`BaseBlockExecutorFactory` for executing blocks with the correct gas rules and precompile sets for
each hardfork. Also provides `AlloyReceiptBuilder` and `BaseReceiptBuilder` for constructing Base
receipts and
`ensure_create2_deployer` for Canyon hardfork compatibility.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-common-evm = { workspace = true }
```

```rust,ignore
use base_common_evm::{BaseEvmFactory, BasePrecompiles, OpSpecId};

let factory = BaseEvmFactory::default();
let precompiles = BasePrecompiles::new_with_spec(OpSpecId::ISTHMUS);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
