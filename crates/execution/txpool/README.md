# `base-execution-txpool`

Base transaction validation, ordering, admission, and forwarding.

`BaseTransactionValidatorBuilder` constructs the single Base validator. Shared Ethereum checks,
L1 data fees, EIP-8130 admission, and validity predicates are handled by that validator.
`BaseOrdering` selects coinbase-tip or timestamp ordering.

`BaseTransactionPool` is the node-facing pool and owns protocol and 2D-nonce admission.
Canonical maintenance runs directly on this pool with `BlockchainProvider` and its canonical
notification stream. The underlying `Pool` provides protocol storage operations; callers do not
need separate validation or maintenance extension traits. Blob storage remains configurable.
No-op pools and mock validators are available only with `test-utils`.

`ValidatedTransaction` carries a recovered sender, an encoded transaction, and flattened
`TransactionValidity` predicates. Empty predicates preserve the legacy JSON layout. Builders
reject non-empty predicates unless validity support is enabled, and enforce the configured
predicate limit. `TxForwardingService::spawn` forwards ordinary transactions;
`spawn_with_extensions` also forwards validity predicates.

Builder, transaction-status, validity-submission, and administrative RPC endpoints live in
`base-execution-rpc`. The pool owns transaction processing and the forwarding client.
