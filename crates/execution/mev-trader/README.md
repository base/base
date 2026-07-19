# Base MEV Trader

`base-mev-trader` is the read-only Phase A measurement engine for in-node MEV analysis.
It captures opaque pending-state snapshots, validates decoded victim frames against that
snapshot, executes only against hash-pinned state, and emits measurement data rather than
transactions.

The crate intentionally contains no network transport, signer, transaction submission, or
transaction-pool integration. Runtime installation is separately gated by the execution CLI.
