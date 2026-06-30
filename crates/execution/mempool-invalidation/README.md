# base-mempool-invalidation

Canonical-state-driven mempool invalidation extension for the Base node.

## Overview

Reth's stock transaction-pool maintenance forwards only per-account nonce and
balance to the pool and drops each committed block's storage diff. This
extension subscribes to the canonical-state broadcast in parallel and feeds the
full per-account `BundleState` deltas (changed balances, protocol nonces, and
storage slots) into the Base transaction pool's exact-match invalidation index.

Channelized EIP-8130 transactions whose watched surface changed — an
actor-config / account-lock slot, a 2D nonce-manager channel slot, the protocol
nonce, or a sponsoring payer's balance — are dropped ahead of the builder via an
`O(watchers)` reverse-index lookup rather than an `O(pool)` rescan.

## Wiring

Installed as a `BaseNodeExtension` on the standard node. It spawns the
`mempool-invalidation` critical task, which runs
`base_execution_txpool::maintain_state_diff_invalidation` against the node's pool
and canonical-state stream. No configuration or CLI flags.
