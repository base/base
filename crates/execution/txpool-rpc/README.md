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

## Validity transaction status

`ValidityTransactionStatusExtension` registers the read-only, builder-local
`base_getValidityTransactionStatus(txHash)` method backed by a
[`base-execution-validity-metadata`](../validity-metadata/README.md) store. It does not change any
`eth_*` method. The lookup resolves the hash through the node's canonical chain provider and
re-checks that the containing block hash is canonical at its height before reading the record for
that exact block hash, so pending, private, orphaned and unknown transactions return `null`.

| Outcome | Response |
| --- | --- |
| Not canonically included | `null` |
| Included, block recorded | `{transactionHash, blockHash, blockNumber, transactionIndex, isValidityTransaction}` with hex quantities |
| Included, no record or metadata disabled | error `-32050`, message `validity metadata unavailable`, data `not_recorded` |
| Corrupt, conflicting or inconsistent record, or storage failure | error `-32603` `Internal error` without diagnostics |

`isValidityTransaction` is `true` only for committed pool transactions that carried nonempty
validity predicates. It is a builder-reported classification at lookup time, not a finality
guarantee or cryptographic proof. A `not_recorded` error may clear once a pending write commits,
but dropped or lost records never recover. Each provider read observes its own canonical
snapshot, so a reorg during a lookup can produce a transient `-32603` (or `null`) instead of a
classification; retry after the chain settles. A conflicting record yields `-32603` permanently by
design; deleting the database is not a routine repair because it discards every other record.

Provider and store reads run on the blocking task pool, at most
`MAX_CONCURRENT_VALIDITY_STATUS_LOOKUPS` (16) at a time; further requests wait for a permit, and a
caller that disconnects keeps its permit until its read finishes. Internal failures are counted
in `builder.validity_metadata.status_lookup_failures` and logged only on the first and every
power-of-two failure, so repeated failing requests cannot flood the logs.

Operator prerequisites: lookups resolve hashes through the node's transaction-hash index and
check the canonical block's body indices. If transaction lookup or bodies are pruned, affected
transactions return `null` even when metadata was recorded, so keep both for the history you
intend to serve; the method is not a generic archival service. Only the block producer that
records the metadata can answer, and only for blocks it built while recording was enabled.
Public deployments should route only this method to that producer rather than exposing the
producer's other RPC methods.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-txpool-rpc = { workspace = true }
```

```rust,ignore
use base_txpool_rpc::{
    SendRawTransactionValidityConfig, SendRawTransactionValidityExtension, TxPoolRpcConfig,
    TxPoolRpcExtension, ValidityTransactionStatusConfig, ValidityTransactionStatusExtension,
};

runner.install_ext::<TxPoolRpcExtension>(TxPoolRpcConfig::default());
runner.install_ext::<SendRawTransactionValidityExtension>(
    SendRawTransactionValidityConfig::default(),
);
runner.install_ext::<ValidityTransactionStatusExtension>(ValidityTransactionStatusConfig {
    store: Some(store.clone()),
});
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
