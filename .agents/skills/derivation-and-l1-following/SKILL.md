---
name: derivation-and-l1-following
description: Work safely on L1 following, derivation, SafeDB persistence, Engine requests, and safe/finalized head transitions.
---

# Derivation and L1 Following

Read `AGENTS.md` and `docs/FEATURE_MAP.md` before using this skill.

## Start here

- `crates/consensus` and `bin/consensus` for L1 following, origins, derivation,
  Engine requests, unsafe gossip, and consensus RPC.
- SafeDB code and tests as part of the derivation persistence and recovery path.

## What derivation does

Derivation turns canonical L1 inputs into the next safe L2 payload attributes.
It advances the derivation pipeline from the last engine-confirmed safe head,
may advance the L1 origin while it consumes input, and sends at most one derived
attributes payload to execution before waiting for that payload's safe-head
confirmation. The transition rules are implemented in
[`state_machine.rs`](../../../crates/consensus/service/src/actors/derivation/state_machine.rs).

## Actors and hand-offs

- [`L1WatcherActor`](../../../crates/consensus/service/src/actors/l1_watcher/actor.rs)
  observes L1 head and finalized streams, fetches the relevant L1 data, and
  notifies derivation. It owns bounded L1-provider retries, not L2 head
  promotion.
- [`DerivationActor`](../../../crates/consensus/service/src/actors/derivation/actor.rs)
  owns pipeline stepping, derivation state, the pending attributes hand-off,
  finalization tracking, and SafeDB updates or resets associated with confirmed
  safe heads and pipeline resets.
- [`EngineActor`](../../../crates/consensus/service/src/actors/engine/actor.rs)
  is the execution hand-off: it receives queued engine requests and delegates
  them to the configured node engine receiver. Derivation must wait for the
  engine-confirmed safe head before producing the next payload attributes.
- The actor exports and client boundaries are collected in
  [`actors/mod.rs`](../../../crates/consensus/service/src/actors/mod.rs). Follow
  those request/client types before changing a channel or moving ownership.

## Operating modes and the execution layer

The consensus service has two `NodeMode` values—`Validator` and `Sequencer`—in
[`service/mode.rs`](../../../crates/consensus/service/src/service/mode.rs).
Follow is a separate consensus service, launched by the `base follow` command,
not a third `NodeMode` value.

| Mode | Consensus source of truth | Relationship to the EL | What it must not own |
| --- | --- | --- | --- |
| **Validator** | L1 data-availability inputs for derivation plus unsafe L2 payloads received through P2P gossip. | The derivation pipeline turns L1 inputs into safe payload attributes and sends them through the Engine API. The EL imports unsafe payloads from the network path, confirms derived payloads as safe, and later receives finalized-head updates. `ValidatorEngineRequestHandler` is selected by the node builder. | Unsafe block production, sequencer signing, or a second safe-head owner. |
| **Sequencer** | Its configured L1-origin selection, local payload-building policy, and the EL state/pool used to build unsafe blocks. | `SequencerActor` asks the EL to build and seal unsafe payloads through the sequencer engine coordinator, then publishes those payloads to P2P gossip. It still uses L1 finalization information to finalize safe blocks. | Treating locally produced unsafe blocks as safe before the derivation/Engine confirmation path establishes safety. |
| **Follow** | Canonical payloads from a configured source L2 RPC, with L1 used to check source origins and optional proof progress used as a gate. | `base follow` launches an execution node and a `FollowNode`; the follow runtime fetches source payloads and inserts them into the local EL over its Engine endpoint, then synchronizes local safe/finalized heads. | L1 derivation, SafeDB ownership, sequencer payload production, or P2P unsafe-block authority. |

The validator and sequencer assembly, including which Engine request handler is
selected, lives in [`service/node.rs`](../../../crates/consensus/service/src/service/node.rs).
The integrated follow launch and its embedded Engine IPC connection live in
[`bin/base/src/commands/follow.rs`](../../../bin/base/src/commands/follow.rs);
the follow node and Engine insert/synchronize behavior live in
[`follow/node.rs`](../../../crates/consensus/service/src/follow/node.rs) and
[`follow/engine.rs`](../../../crates/consensus/service/src/follow/engine.rs).

## Event messages and responses

The request enum in
[`derivation/request.rs`](../../../crates/consensus/service/src/actors/derivation/request.rs)
is the narrow event boundary for the derivation actor. Preserve both the message
meaning and the response it triggers:

| Event source | Message to derivation | Required response and ownership |
| --- | --- | --- |
| `L1WatcherActor` receives a usable L1 head | `ProcessL1HeadUpdateRequest` | Mark `L1DataReceived`, then attempt derivation. If the pipeline needs more data, transition to `AwaitingL1Data`; do not invent a safe-head update. |
| `L1WatcherActor` receives an L1-finalized block | `ProcessFinalizedL1Block` | Update the finalizer. If a queued derived L2 block becomes eligible, send its number to execution as finalized; otherwise retain the finalized signal for a later confirmation. |
| Execution completes initial sync | `ProcessEngineSyncCompletionRequest` | Reset/re-anchor SafeDB at the engine safe head, record `ELSyncCompleted`, then begin derivation. This is the point at which derivation may leave `AwaitingELSyncCompletion`. |
| Execution confirms a derived safe head | `ProcessEngineSafeHeadUpdateRequest` | Record the L1-inclusion-to-safe-L2 mapping in SafeDB, apply `NewAttributesConfirmed`, retry pending finalization, then derive at most the next attributes payload. |
| Execution requests a pipeline signal such as reset or flush | `ProcessEngineSignalRequest` | Signal the pipeline and record `SignalProcessed`. A reset clears in-flight finalization and pending inclusion state; SafeDB reset failure is observable and must not silently change ownership. |
| The pipeline yields attributes | `send_safe_l2_signal` to the engine client | Record `NewAttributesDerived`, enqueue finalization tracking, retain the L1 inclusion block, and wait in `AwaitingSafeHeadConfirmation` for engine confirmation before deriving again. |

See the request handling and outbound engine hand-off in
[`derivation/actor.rs`](../../../crates/consensus/service/src/actors/derivation/actor.rs).

## Follow is a distinct path

Follow does not derive L2 payload attributes from L1. A
[`FollowNode`](../../../crates/consensus/service/src/follow/node.rs) fetches
source L2 payloads, inserts them into its local execution engine, and updates
local safe/finalized heads. The engine-side hand-offs are implemented in
[`follow/engine.rs`](../../../crates/consensus/service/src/follow/engine.rs).
Do not move L1 origin, SafeDB, or derivation-state ownership into the follow
path merely because both paths update execution heads.

## Flow and ownership

Trace L1 source → `L1WatcherActor` → derivation origin and pipeline →
`DerivationActor` attributes request → `EngineActor`/execution result →
engine-confirmed safe head → SafeDB persistence and status. Separately, trace
the follow path as remote L2 source → `FollowNode` runtime → local Engine API →
local safe/finalized heads. Model legal and rejected transitions before changing
code; name the sole owner of every head promotion, persistence write, retry, and
recovery decision.

## Invariants and failure modes

Preserve monotonic origin/head relationships, duplicate and reordered event
handling, reorg rollback, exactly-once side effects, and restart recovery from
persisted SafeDB state. Do not let multiple layers promote a head or own the
same recovery decision.

## Validation

Cover the triggering transition plus its duplicate, rollback, or failure case.
Use integration evidence whenever the contract crosses L1, Engine, persistence,
reorg, or restart boundaries.
