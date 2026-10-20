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

## Swap Lane ingress

`SendSwapTransactionExtension` registers `base_sendSwapTransaction` and
`admin_setCandidateSelectionEnabled` when candidates are enabled. The endpoint parses the
request, resolves `maxAccessCharge`, routes on the selection kill switch, builds the response
and journals ingress events. Admission itself runs in the process-shared
`base_execution_txpool::AdmissionService`. That service owns in-flight duplicate waits,
cancellation settlement, policy checks and pool insertion, so every adapter that admits
candidate groups sees the same in-flight state.

The sender signs no expiry. A candidate group expires at a node-assigned block: the
configured group lifetime past the next block, rounded up to whole blocks at the active
full-block cadence. A default transaction submitted alone carries no validity predicate. It
stays in the pool like any other Private transaction until it lands, is replaced, has its
nonce consumed, or is evicted by pool limits.

Delivery. Admitted transactions use Private origin, so devp2p never gossips them, and today
only builders install the endpoint. Builders run no transaction forwarding, so a submission
reaches only the builder that received it. In a high-availability set only the leader
builds, so a submission sent to a follower waits there until that node leads. Transaction
forwarding skips candidate groups, so even on a forwarding node a group would stay local.
Proxy mode (`sequencer_url`) forwards every request unchanged to one upstream node.
The planned direction is mempool fan-out: install the endpoint on mempool nodes, have
transaction forwarding carry admitted singles, and add a group-aware extension to
`base_insertValidatedTransaction` so every builder receives candidate groups.
`AdmissionService` and `GroupDescriptor` do not depend on the swap RPC, so a mempool node's
endpoint and a builder's trusted insert path can admit through them.

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
