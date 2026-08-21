# `base-txpool-rpc`

Transaction pool RPC APIs for Base.

Provides RPC endpoints for submitting transactions, querying transaction status, and managing the
transaction pool.

## Overview

Exposes JSON-RPC APIs for transaction pool administration and transaction lifecycle tracking.
`AdminTxPoolApiImpl` provides admin-level pool management, while `TransactionStatusApiImpl`
allows clients to query the current status of individual transactions by hash. The separate
`SendRawTransactionValidityExtension` registers local mempool ingress through
`base_sendRawTransactionValidity`; typed validity predicates are preserved while forwarding to
builders. This endpoint is experimental, but predicates are now evaluated and enforced by the
builder during block construction: a transaction is only included at a point where all of its
predicates hold, and it is evicted once it can no longer be included.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-txpool-rpc = { workspace = true }
```

```rust,ignore
use base_txpool_rpc::{
    DEFAULT_MAX_VALIDITY_PREDICATES, SendRawTransactionValidityExtension, TxPoolRpcConfig,
    TxPoolRpcExtension,
};

runner.install_ext::<TxPoolRpcExtension>(TxPoolRpcConfig::default());
// Install only when the node's explicit experimental validity flag is enabled.
// The config is the maximum number of validity predicates accepted per transaction.
runner.install_ext::<SendRawTransactionValidityExtension>(DEFAULT_MAX_VALIDITY_PREDICATES);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
