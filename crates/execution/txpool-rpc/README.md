# `base-txpool-rpc`

Transaction pool RPC APIs for Base.

Provides RPC endpoints for submitting transactions, querying transaction status, and managing the
transaction pool.

## Overview

Exposes JSON-RPC APIs for transaction pool administration and transaction lifecycle tracking.
`AdminTxPoolApiImpl` provides admin-level pool management, while `TransactionStatusApiImpl`
allows clients to query the current status of individual transactions by hash. The separate
`SendRawTransactionValidityExtension` registers local ingress through
`base_sendRawTransactionValidity` on forwarding ingress nodes and builders. Typed
validity predicates are preserved in the pool (and while forwarding to builders). This endpoint
is registered at startup but accepts validity-bearing submissions only at Cobalt activation
(or earlier with the experimental override). Predicates are enforced by the builder during
block construction; an unsatisfied transaction is deferred and an expired one is evicted.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-txpool-rpc = { workspace = true }
```

```rust,ignore
use base_txpool_rpc::{
    SendRawTransactionValidityConfig, SendRawTransactionValidityExtension, TxPoolRpcConfig,
    TxPoolRpcExtension,
};

runner.install_ext::<TxPoolRpcExtension>(TxPoolRpcConfig::default());
// The endpoint is pre-registered; the override permits validity before Cobalt.
runner.install_ext::<SendRawTransactionValidityExtension>(
    SendRawTransactionValidityConfig { experimental_override: true, ..Default::default() },
);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
