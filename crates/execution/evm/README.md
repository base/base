# `base-evm`

Base EVM implementation.

## Overview

Provides Base-specific EVM execution support. Maps hardfork activation timestamps to revm
`SpecId` values, and exposes `OpEvm`, `OpEvmFactory`, `OpBlockExecutor`, and
`OpBlockExecutorFactory` for executing blocks with the correct gas rules and precompile sets for
each hardfork. Also provides `OpAlloyReceiptBuilder` for constructing OP receipts and
`ensure_create2_deployer` for Canyon hardfork compatibility.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-evm = { workspace = true }
```

```rust,ignore
use base_evm::{OpEvmFactory, spec_by_timestamp_after_bedrock};

let spec = spec_by_timestamp_after_bedrock(timestamp);
let factory = OpEvmFactory::default();
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
