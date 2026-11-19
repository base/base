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
while forwarding to builders. The endpoint is registered at startup. Use one shared
`--validity-signature-mode off|verify-if-present|required` option on ingress and builders.
It defaults to `off`, accepting unsigned or signed submissions without signature verification
and preserving supplied signatures for forwarding. Deploy `verify-if-present` fleet-wide
to accept unsigned submissions while checking all supplied signatures at raw ingress, migrate clients,
then enable `required` to reject unsigned predicates. Neither `off` nor optional verification
protects against signature stripping and unsigned resubmission.
The trusted `base_insertValidatedTransaction` endpoint checks signature presence only;
it does not re-verify signatures. Protection requires every forwarding ingress node
and each builder's raw ingress to run `required`, builders to require signatures on
insert, and the insert endpoint to be restricted to trusted forwarders. A `required`
builder receiving from an `off` forwarder still accepts invalid supplied signatures.
Predicates are enforced during block construction; an unsatisfied transaction is deferred
and an expired one is evicted. Regular RPC nodes with a configured sequencer URL proxy
the complete sidecar and upstream error response instead of admitting it locally.
See the [wallet signing contract](../txpool/README.md#signed-validity-predicates) for
EIP-712 types, signature encoding, rollout requirements, and security limitations.

## Alternative validity conditions

`base_sendRawTransactionValidity` takes one signed transaction and an options object. A flat
`validity` array still requires **all** predicates. To accept **any complete group**, send
`validity: {"anyOf": [[A, B], [C, D]]}`. For example, the following options accept a transfer if
account `0x11…11` has a positive balance through block 100, **or** its nonce is 2 through block 105:

```json
{
  "validity": {
    "anyOf": [
      [
        {"type":"balance","params":{"address":"0x1111111111111111111111111111111111111111","op":">","value":"0x0"}},
        {"type":"block_number","params":{"op":"<=","value":"0x64"}}
      ],
      [
        {"type":"nonce","params":{"address":"0x1111111111111111111111111111111111111111","op":"=","value":"0x2"}},
        {"type":"block_number","params":{"op":"<=","value":"0x69"}}
      ]
    ]
  }
}
```

Use block bounds within the node's current validity window. Every group needs its own upper
bound. Empty groups and nested expressions are rejected; the configured predicate limit applies
to the sum across groups. Both groups refer to the same transaction hash, so if both hold,
there is still only one transaction to include. Upgrade proxies, ingress nodes and builders
before sending grouped requests; nodes that expect a flat array cannot decode them.

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
