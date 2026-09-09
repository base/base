# `base-common-evm`

EVM implementation.

## Overview

Provides Base-specific EVM execution support. Maps upgrade activation timestamps to revm
`SpecId` values, and exposes `BaseEvm`, `BaseEvmFactory`, `BaseBlockExecutor`, and
`BaseBlockExecutorFactory` for executing blocks with the correct gas rules and precompile sets for
each upgrade. Execution produces `BaseReceipt` directly, including deposit metadata and EIP-8130
phase statuses. Bloom generation happens when receipts are encoded.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-common-evm = { workspace = true }
```

```rust,ignore
use base_common_evm::{BaseEvmFactory, BasePrecompiles, BaseSpecId, BaseUpgrade};

let factory = BaseEvmFactory::default();
let precompiles = BasePrecompiles::new_with_spec(BaseSpecId::new(BaseUpgrade::Beryl)).install();
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
