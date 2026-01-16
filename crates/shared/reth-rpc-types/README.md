# `base-reth-rpc-types`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Re-exports reth RPC types for external consumers. Provides a stable interface for external projects to depend on reth types without directly depending on the reth crates.

## Overview

- **`EthApiError`**: Error type for Ethereum RPC API operations (from `reth-rpc-eth-types`).
- **`SignError`**: Error type for transaction signing operations (from `reth-rpc-eth-types`).
- **`extract_l1_info_from_tx`**: Utility to extract L1 block info from deposit transactions (from `reth-optimism-evm`).

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-reth-rpc-types = { git = "https://github.com/base/base" }
```

Use the re-exported types:

```rust,ignore
use base_reth_rpc_types::{EthApiError, SignError, extract_l1_info_from_tx};

// Handle RPC errors
fn handle_error(err: EthApiError) {
    match err {
        EthApiError::InvalidParams(msg) => println!("Invalid params: {msg}"),
        _ => println!("Other error: {err}"),
    }
}

// Extract L1 info from a deposit transaction
let l1_info = extract_l1_info_from_tx(&deposit_tx)?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
