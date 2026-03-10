# `base-txpool-rpc`

Transaction pool RPC APIs for Base.

Provides RPC endpoints for querying transaction status and managing the transaction pool.

## Overview

Exposes JSON-RPC APIs for transaction pool administration and transaction lifecycle tracking.
`AdminTxPoolApiImpl` provides admin-level pool management, while `TransactionStatusApiImpl`
allows clients to query the current status and lifecycle events of individual transactions
by hash. Both implement jsonrpsee server traits and are registered as node RPC extensions.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-txpool-rpc = { workspace = true }
```

```rust,ignore
use base_txpool_rpc::{AdminTxPoolApiImpl, TransactionStatusApiImpl, TxPoolRpcExtension};

let ext = TxPoolRpcExtension::new(pool.clone(), tracker.clone());
rpc_module.merge(ext.into_rpc())?;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
