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
validity predicates and their optional `validity_signature` are preserved in the pool and
while forwarding to builders. The endpoint is registered at startup. Signature enforcement
is default-off: opt in with `--validity-require-signature` on forwarding ingress nodes and
every builder. On builders, this single flag covers both raw and forwarded ingress.
Enforcement rejects unsigned predicates, tampered sidecars, and signatures from another key.
When off, legacy unsigned submissions remain accepted and signed sidecars are rejected.
Predicates are enforced during block construction; an unsatisfied transaction is deferred
and an expired one is evicted. Regular RPC nodes with a configured sequencer URL proxy
the complete sidecar and upstream error response instead of admitting it locally.
See the [wallet signing contract](../txpool/README.md#signed-validity-predicates) for
EIP-712 types, signature encoding, rollout requirements, and security limitations.

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
runner.install_ext::<SendRawTransactionValidityExtension>(
    SendRawTransactionValidityConfig::default(),
);
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
