# S1-b runtime simulation/live switch and four-lock egress

Status: proposed for review  
Base: `rebase/v1.1.1-beryl` at `a2180921`

## 1. Scope and invariants

S1-b makes simulation the production default backend and permits a runtime-selected live backend only inside the existing `arm-live-egress` compile feature. It does not create a second candidate, assembly, signing, or economics path.

The invariant hierarchy is:

1. A build without `arm-live-egress` contains no `ProdBackend`, `reqwest`, socket opener, or live runtime variant.
2. A build with `arm-live-egress` still defaults to simulation. Live selection requires the explicit node flag described below.
3. Every live attempt passes one four-lock evaluation immediately after the existing egress-moment freshness revalidation and before a linear egress permit is minted.
4. `RawBackend` remains sealed. Production has exactly two implementations: `SimBackend` and, only under `arm-live-egress`, `ProdBackend`.
5. Simulation and live consume the same `SubmissionAttempt`, `ProofBindings`, `ValidatedExecutionIdentity`, request builders, and strictly-positive priority-economics admission.

`arm-live-egress` remains the outer compile gate. The runtime switch does not replace or weaken it.

## 2. Production simulation backend

The current test-only `FakeBackend` is not promoted with its configurable success/failure controls. Instead, its recording behavior becomes a production `SimBackend` with no network client and no configurable transport result.

`SimBackend` consumes the same linear `RawEgress` plan as the live backend. It converts the plan into a `SimulationRecord` containing:

- whether the attempt was initial or attribution-only;
- each inert request's channel, method, compile-pinned endpoint, and exact body bytes;
- the expected inclusion hash or prior inclusion receipt hash as appropriate.

The record is returned by value to the caller in `SubmitOutcome::Simulated(SimulationRecord)`. It is not logged and is not written to a process-global buffer, file, database, or socket. The caller decides whether and where to persist it. This keeps the backend deterministic, allocation-bounded by the already-owned request bodies, and free of hidden retention.

`SubmitOutcome::Complete` is split so simulated and real completion cannot be confused:

- `Simulated(SimulationRecord)` means no transport was attempted;
- `LiveComplete` means both live channels completed;
- the existing no-egress and live partial/failure outcomes remain explicit.

Tests for live partial failures use a helper that exists only under `#[cfg(test)]` and accepts injected test closures only in that configuration. The production `ProdBackend` call path takes no caller-supplied behavior. Removing the test-only cfg is a committed RED mutant. This does not add configurable behavior to `SimBackend` or another `RawBackend` implementation.

## 3. Runtime switch source

The exact runtime source is a new clap boolean on the node command:

```text
--mev-live-egress
```

It has no environment-variable alias, configuration-file fallback, or implicit value. Absence is `false`. The field and the live runtime variant compile only when the node-to-submit feature chain ends in `mev-trader-submit/arm-live-egress`; the default node/workspace feature closure does not contain that chain.

The parsed boolean is reduced once during MEV runtime installation to a non-`Clone` runtime backend selection:

- false: `RuntimeBackend::Simulated(&SimBackend)`;
- true: construction of the live selection is attempted, but only in an `arm-live-egress` build.

There is no mutable runtime setter. Restarting with the explicit flag is required to request live mode. Selecting live does not itself authorize egress; it supplies only lock L1 below.

The signed receipt is the existing owner-verified `LiveRunAttestation`, not a new receipt format. Its verified campaign and window are already consumed into `AuthorizedCandidate` and retained in private `ProofBindings`. The existing egress freshness check revalidates that window. No caller can construct `ProofBindings` or substitute an unsigned receipt.

## 4. One four-lock evaluation

`send_gated` remains the single submission entry. For both initial attempts and attribution retries it performs these steps in order:

1. consume the attempt and run the existing full `FreshnessSources` revalidation;
2. if simulation is selected, mint the ordinary linear `RawEgress` and return a `Simulated` record;
3. if live is selected, gather and evaluate L1-L4 below in one private function;
4. only an all-open result constructs `LiveEgressPermit { private: () }`;
5. consume that non-`Clone` permit and `RawEgress` in `ProdBackend::execute`.

The live locks are:

- **L1 — explicit live selection:** evidence is derived only by consuming the non-`Clone` live `RuntimeBackend` selection produced from the present `--mev-live-egress` startup flag. It is not a caller-supplied boolean. Simulation is the default.
- **L2 — signed arm receipt:** the private bindings came from a verified `LiveRunAttestation`, cover the same campaign, and remain inside their signed time window at the egress recheck.
- **L3 — kill anchor:** the existing authoritative `ArmedFailSink::observe_kill` returned `KillState::Clear` during that same freshness recheck. Unknown, engaged, read failure, or process poison closes the attempt.
- **L4 — funds cap:** a fresh canonical committed-state read of the compile-pinned funded hot wallet succeeds and reports a native ETH balance less than or equal to the signed `ArmedCriteria::hot_wallet_cap_wei()`.

The private evaluator receives an inspectable `LiveLockSnapshot` and returns either all-open or a typed closed reason. It is pure and total. Production code gathers L1 only by consuming the non-`Clone` live runtime selection; there is no constructor that accepts a free boolean. Tests mutate cloned serialized fixtures, not production capability values.

Evaluation belongs after freshness revalidation because L2 and L3 are already authoritative members of that egress-moment conjunction. Moving it earlier would create a stale authorization window. It belongs before `RawEgress` reaches `ProdBackend` so no network-capable function can receive an attempt without the linear live permit.

`LiveEgressPermit` has private fields, no public constructor, no `Clone`/`Copy`, and one construction expression in `send_gated`. `ProdBackend::execute` requires it by value. Simulation neither constructs nor accepts it.

## 5. Funds authority and immutable cap

The cap and balance are both denominated in wei of native ETH:

- **Cap:** `hot_wallet_cap_wei` from the already owner-signed, compile-SHA-pinned `ArmedCriteria` payload. There is no CLI, environment, file, or API override, so runtime code cannot widen it.
- **Balance:** native balance of the compile-pinned `FUNDED_WALLET` at the latest committed canonical head.

`CommittedStateAuthority` gains a node-local `native_balance_at_latest_committed(Address) -> Result<Option<U256>, ProviderError>` operation. `Some(balance)` proves that the compile-pinned account was present at the latest committed canonical head; `None` is absence, not zero. The existing `ProductionCodeHashProvider` is also the funds source, so code hash, head height, account presence, and balance use the same canonical database authority; no RPC/network balance reader is introduced. `FreshnessSources` borrows this source.

Account absence closes live egress alongside authority unavailability, decode/provider errors, a non-canonical read, arithmetic/type failure, or a present balance above the signed cap. A present account with a genuine zero balance may pass L4 and is pinned by a GREEN seal case. L4 intentionally caps total hot-wallet exposure, not trade size: holding more than the signed cap stops live egress, while signed `per_tx_cap_wei` separately bounds each trade. The check does not truncate or clamp.

## 6. Positive-EV dust filter

S1-b reuses the existing ex-ante `PriorityEconomicsAuthority` and `economics::evaluate` gate. That gate requires same-block simulated execution gas, OP L1 data fee, Base fee, victim priority/max fee, checked arithmetic, and strictly positive `expected_ev_wei` before `ValidatedUnsignedAtomicTx` exists.

No backend accepts an unvalidated candidate. Both modes consume the same later `PairedSubmission`, so runtime selection cannot bypass or recompute economics. S1-b adds seals proving both backend branches are downstream of the one `RawEgress` construction and that no backend directly accepts candidate or unsigned transaction types.

Dust size remains bounded by the signed `per_tx_cap_wei`; this change adds no second or runtime-widenable size limit.

## 7. Feature and API shape

The default feature graph remains unchanged. The live feature chain is default-off and explicit from node to CLI to submit crate. In the submit crate:

- `SimBackend`, simulation records, and the simulated runtime variant compile with `arm`;
- `ProdBackend`, the live runtime variant, `reqwest`, response parsing needed only by live transport, and every socket call site remain under `#[cfg(all(feature = "arm-live-egress", not(test)))]` where applicable;
- tests may compile pure live response helpers under `cfg(test)`, but no test or simulation backend opens a socket.

The unnameable `sealed::Sealed` supertrait remains. The only production `RawBackend` impl blocks are the always-available `SimBackend` and feature-gated `ProdBackend`.

## 8. Seals and committed mutants

### 8.1 Pure four-lock suite

`evaluate_live_locks(&LiveLockSnapshot) -> Result<(), LiveLockClosed>` is the pure production evaluator. The committed mutation harness represents the same snapshot as a `serde_json::Value` and passes it through `validate_live_lock_fixture(&Value) -> Result<LiveLockSnapshot, String>` before evaluation. Every field is required and type-checked, enum strings use exact allowlists, unknown fields/variants are rejected, and no entry is skipped. The suite patches clones and asserts every patch changed its input.

| Case | Mutation | Expected |
|---|---|---|
| L0 | all four locks open, balance exactly at cap | GREEN |
| L0z | all four locks open, account present with zero balance | GREEN |
| L1 | explicit-live false | RED |
| L2 | signed receipt absent, mismatched, not-yet-valid, or expired | RED |
| L3a | kill unknown | RED |
| L3b | kill engaged/poisoned | RED |
| L4a | funded account absent | RED |
| L4b | balance authority error | RED |
| L4c | balance is cap + 1 wei | RED |

An additional GREEN case pins default simulation with no live evaluation and a typed `Simulated` outcome. This distinguishes a safe default from an evaluator that rejects everything.

### 8.2 Structural/source seals

The live `arm_capability_seal` and T4b closure seals assert:

1. `cargo tree --workspace -i mev-trader-submit -e features` positively contains `mev-trader-submit feature "default"` and contains none of `arm`, `arm-provisioning`, or `arm-live-egress`.
2. `ProdBackend`, its `RawBackend` impl, `reqwest`, `.send()`, and the live runtime variant remain behind the exact outer feature gate.
3. `SimBackend` has no network/process/filesystem/logging token and returns only `SubmitOutcome::Simulated`.
4. `SubmitOutcome` has distinct simulated and live completion variants; an ambiguous `Complete` variant is forbidden.
5. `RawBackend` still has the unnameable sealed supertrait and exactly the reviewed production implementations. Duplicate or target-scoped impl/dependency entries fail counted checks rather than being deduplicated.
6. `LiveEgressPermit` has private fields, no public constructor, no `Clone`/`Copy`, one construction site, and is consumed by the live execute call.
7. The live CLI flag defaults false, has no env alias, and is absent unless the live compile feature is selected.
8. The funds address is `FUNDED_WALLET`; the cap comes only from `ArmedCriteria::hot_wallet_cap_wei`; balance errors and over-cap values close.
9. Both runtime branches remain downstream of the existing positive-EV authority and cannot accept pre-validation candidate types.

Committed structural mutants remove each production cfg, remove the `#[cfg(test)]` from the closure-injection seam, introduce a socket token in simulation, add an ambiguous completion, add a backend impl, duplicate a linker/impl entry, make the live permit constructible or cloneable, add a free-bool L1 constructor, add a cap override, change the funded address, bypass the priority filter, or make the live flag default true. Every mutant must be RED and must assert that its source/metadata fixture actually changed. Unmodified fixtures are GREEN.

The structural harness likewise normalizes Cargo metadata and `syn` inventories into a `serde_json::Value` consumed by `validate_runtime_switch_seal(&Value) -> Result<(), String>`. Missing, mistyped, duplicate, target-scoped, or unclassified package/target/impl entries return `Err`; they are never ignored with `continue`.

## 9. Failure behavior

Any failed freshness predicate returns `NoEgress`. Simulation selection returns `Simulated` only after the existing freshness check. A requested live selection with any L1-L4 failure returns a typed no-egress/lock-closed outcome and does not silently fall back to simulation; silent fallback would misreport the requested mode. Balance and receipt read failures are terminal for that attempt but do not invent a new durable latch. Existing fail-stop/kill behavior remains authoritative.

Attribution retries repeat freshness and all live locks. A live retry never re-sends inclusion. A simulated retry records only the attribution request and its bound inclusion receipt hash.

## 10. Non-goals and remaining owner acts

This package adds source code and tests only. It does not:

- create or validate a new arm receipt format;
- arm `ArmedCriteria` or alter its signed pins;
- sign an owner receipt or transaction as an operational act;
- provision the suppression JSON writer;
- clear or bypass the kill anchor;
- fund the hot wallet;
- enable `arm-live-egress` in any default/workspace/node artifact;
- start a node, open a socket, submit a transaction, or contact a relay.

Before a real live attempt, the owner still must build the explicitly live-capable artifact, provide the existing signed arming and live-run evidence, ensure suppression and deployment evidence are fresh, keep the kill anchor clear, fund no more than the signed cap, start with `--mev-live-egress`, and separately authorize deployment/restart. Those operational acts are outside both S1-b PRs.

S1-b deliberately leaves `send_gated` without a production caller and returns simulation records without persistence. The named follow-up package is **simulation entrypoint + record persistence**: wire the existing unified path to call the default simulation backend and persist `SimulationRecord` at an owner-reviewed bounded sink. Until that package lands, S1-b produces no simulation datum.
