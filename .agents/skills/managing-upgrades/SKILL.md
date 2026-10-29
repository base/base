---
name: managing-upgrades
description: "Implements and reviews Base hardfork-gated protocol behavior using the feature registry. Use for activation-sensitive or consensus-critical changes across derivation, execution, sequencing, batching, RPC, and proofs; not benchmark-only or non-protocol changes."
---

# Managing upgrades

Keep Base protocol features, their hardfork activations, and cross-stack documentation consistent.

1. Read [the upgrade guide](../../../docs/guides/UPGRADES.md) and the relevant entries in
   [BaseFeature](../../../crates/common/genesis/src/feature.rs). Trace their consumers
   before editing. Distinguish Ethereum spec checks from Base protocol behavior.
2. Gate behavior using a semantic `BaseFeature`. Reuse an existing feature only for
   the same protocol change, including its input parameters and transition work,
   not merely the same activation date. Do not split a cohesive rule into one
   feature per implementation layer. The Rust registry owns each feature-to-upgrade
   mapping; no second mapping belongs in consumers or docs.
3. Preserve the consumer's activation adapter: rollup `is_feature_active_cascading`,
   execution `is_feature_active_at_timestamp`, or resolved EVM spec. For boundaries,
   distinguish the rollup's `configured_feature_activation_timestamp` from the
   execution trait's resolved timestamp. Use the actual parent timestamp for
   transition checks. Keep genesis activation, first-block layout exceptions,
   runtime `Never`, schedule rescheduling, and historical proof pinning intact.
   Never add mutable per-node switches for consensus rules.
4. Check all applicable paths: batcher/derivation, sequencer/builder, validation and
   re-execution, RPC/txpool, and proof execution. Preserve pre-fork behavior. For a
   new upgrade follow the guide's enum/config/contract-order checklist. Keep
   Ethereum compatibility mapping separate from Base behavior selection.
5. Test observable results before, at, and after activation. Include absent/genesis
   activation, sparse or non-monotonic schedules, runtime overrides, and the actual
   parent where relevant. Reuse existing public-API fixtures. Run affected crate
   tests and `no_std` checks for shared/proof dependencies.
6. Run `python3 etc/scripts/local/upgrade-docs.py sync`. Review the changed feature
   contracts, linked production code, and each affected upgrade's prose. Review
   and update prose whenever activation semantics or feature behavior changes,
   even if feature names and generated tables stay unchanged.
   Run `just check::upgrade-docs`. It validates registry coverage, generated tables,
   links, and common named-fork gate bypasses; it does not verify semantic freshness.
   Protocol review and behavior tests must establish agreement with the prose.

Fork names are still appropriate in schedule machinery, wire/config identifiers,
upgrade transaction bundles, Ethereum compatibility maps, and historical fixtures.
Production decisions about Base behavior must use features. Benchmark-only work
does not require this workflow unless it also changes protocol behavior.
