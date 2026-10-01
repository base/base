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
Regular RPC nodes with a configured sequencer URL proxy this method, including predicates and
the upstream error response, rather than admitting the transaction into their local pool.

## Nonce predicates

The `nonce` predicate compares an account's nonce in the builder's current execution state.
It accepts `address`, `op`, and `value`, with the same operators as `balance`:
`<`, `<=`, `=`, `!=`, `>`, and `>=`. Values use hexadecimal quantity encoding.
An account that does not exist has nonce zero.

For example, add this predicate to the request's `validity` array to wait until the watched
account's nonce exceeds `42`:

```json
{
  "type": "nonce",
  "params": {
    "address": "0x1111111111111111111111111111111111111111",
    "op": ">",
    "value": "0x2a"
  }
}
```

Include the required `block_number` upper-bound predicate (`<`, `<=`, or `=`) in the same
array, within the configured expiry window. All predicates must hold before execution.
If the nonce condition is false, the transaction waits; a change to the watched nonce can
make it eligible later in the same block build. The comparison uses executed state, not
pending transactions in the pool.

A nonce condition establishes that the account nonce has advanced, rather than proving a
particular transaction hash succeeded: a replacement or reverted transaction also consumes
its sender's nonce, and account nonces can change through contract creation or EIP-7702
authorizations. Inclusion in the current build does not imply finality.

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
