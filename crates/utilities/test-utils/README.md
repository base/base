# `base-test-utils`

Shared test utilities for integration testing across Base crates.

## Overview

Provides hardcoded test accounts, genesis configuration, and Solidity contract bindings for use
in integration tests. Exports `build_test_genesis` for constructing a dev genesis state,
pre-funded `Account` instances, and ABI-generated factories for test contracts (`MockERC20`,
`SimpleStorage`, `Proxy`, `TransparentUpgradeableProxy`, `Logic`, `AccessListContract`, and
others).

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dev-dependencies]
base-test-utils = { workspace = true }
```

```rust,ignore
use base_test_utils::{build_test_genesis, DEVNET_CHAIN_ID};

let genesis = build_test_genesis();
let chain_spec = OpChainSpec::from_genesis(genesis);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
