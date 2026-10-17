# Fill-immediately-or-kill validity transactions

This document defines the `fill_immediately_or_kill` flag on validity transactions: what it
promises, how the builder and the pool decide it, how it behaves in the searcher flows it is meant
for, and what it deliberately does not do. Read it before changing the flag's semantics, the
predicate write journal, or the pool-side expiry.

## Problem

A validity transaction bids on a state transition described by balance, nonce, and storage
predicates. Searchers racing for the same opportunity submit transactions whose predicates stay
true after a competitor lands, because the bounds that would capture the opportunity exactly
(pool reserves, active liquidity ranges, limit orders, hook state) are hard to write up front. The
loser's predicates still hold, so it lands behind the winner and pays gas for nothing. Searchers
respond by discounting their bids.

L1 solves this with revert protection. Base does not offer it: it needs the sequencer to execute a
transaction, inspect the result, and roll it back, which is expensive at 200ms blocks and ties
inclusion to a trusted builder. The flag gives the useful part of revert protection without
execution: the builder drops a flagged transaction when the state it conditioned on moved before
it could land.

## Definitions

- **Predicate locations** `K(S)`: the balance, nonce, and storage locations read by the state
  predicates of a transaction `S`. Block-number and flashblock-index predicates read the build
  position, not state, and are not locations.
- **Write**: a committed transaction changes the value at a location. A transaction that reads a
  location, or writes it back to the same value, does not write it. A reverted transaction writes
  only its sender's nonce and balance.
- **Satisfied at position `p`**: every predicate of `S` holds against the state after the first
  `p` pool transactions of the block (and the build position in effect there). Position `0` is
  the state after the block's sequencer transactions.

## Contract

A flagged transaction `S` is **filled** only in the first block in which its predicates are
satisfied, and only if no transaction between the position where they first became satisfied and
`S`'s own position wrote a location in `K(S)`. Otherwise it is **killed**: dropped without
executing, evicted from the pool, and never retried. To bid again, submit a new transaction.

Spelled out for one block build:

| `S`'s predicates | Writes to `K(S)` | Outcome |
| --- | --- | --- |
| Not satisfied anywhere in the block | any | parked; waits for a later block |
| First satisfied at position `p0`, `S` reached at `p >= p0` | none in `(p0, p]` | **filled** at `p` |
| First satisfied at position `p0`, `S` reached at `p >= p0` | at least one in `(p0, p]` | **killed**: state overwritten |
| Satisfied at some position, block closes without `S` | any | **killed**: unfilled |

The write that makes the predicates true is not a conflict: a backrun's trigger is exactly the
write it waits for. Writes by any transaction count, flagged or not, validity or not, including
ordinary user transactions, sequencer transactions, and the sender's own earlier transactions.

Three properties follow and are what reviewers should check against:

1. **Read-after-write only.** Another transaction that merely reads the same locations never
   kills `S`. Two liquidations reading one oracle slot, or two backruns reading one victim's nonce,
   do not exclude each other unless one of them writes a location the other reads.
2. **Independent of the scan.** The decision is a function of the block's committed transaction
   sequence and `S`'s predicates. It does not depend on which flashblock first considered `S`,
   on where `S` sat in the priority order, or on when `S` reached the builder within the block.
   The builder reconstructs the predicate state at every earlier position where it could have
   changed, so a transaction that arrives after the competitor's flashblock is judged the same as
   one that was there from the start.
3. **Derivable from canonical state.** Whether `S` was satisfiable in a canonical block is
   recomputed by every pool from the block's final state, so a kill does not depend on the
   process that built the block.

## How the builder decides

Both builders (`crates/builder/core`, the flashblocks sequencer builder, and
`crates/execution/payload`, the native builder) share one implementation in
`crates/execution/payload/src/fill_or_kill.rs`.

**Predicate write journal.** After each pool transaction commits, the builder records the state
it changed: for every balance, nonce, and storage slot whose value differs from before the
transaction, the journal stores the location's value at block start (the first time it is written)
and the `(transaction index, new value)` of each write. It also records the position at which each
flashblock started. This is the same per-commit pass that already wakes parked predicates, so the
extra cost is one hash-map insert per changed location.

**Kill check.** When a flagged transaction's predicates hold at the current position, the builder
collects the block positions where their truth could have changed: block start, each flashblock
start, and the state after each recorded write to a location in `K(S)`. It evaluates the
predicates at each such position in order, reading historical values from the journal and current
values for untouched locations, until it finds the first position `p0` at which they held. If any
location in `K(S)` was written at or after `p0`, the transaction is killed. No EVM execution is
involved; the check costs `|K(S)|` comparisons per candidate position and there are as many
candidates as writes to `K(S)` in the block.

**Block close.** Every flagged transaction whose predicates were satisfied during the block and
which was not included is killed when the block closes. In the flashblocks builder this happens
when the payload is finalized; in the native builder, at the end of its single scan, applied only
if that payload is returned for use.

**Eviction.** A killed transaction is removed from the builder's pool and recorded in the
builder's rejection cache, so a mempool node re-forwarding the same hash a few seconds later cannot
re-admit it before the pool-side rule below catches up. Kills are reported as `BUILDER_EXPIRED`
transaction events with `fill_immediately_or_kill_state_overwritten` or
`fill_immediately_or_kill_unfilled`.

## How the pool decides

The builder's decision lives in one process. After a sequencer failover the new leader's pool may
still hold a transaction the old leader killed, and a mempool node keeps re-forwarding what it
holds. Both would revive a transaction whose loose predicates still hold. The pool therefore
derives the same decision from canonical state (`crates/execution/txpool/src/fill_or_kill.rs`).

Every pool records the canonical head `H` at which it admitted a flagged transaction. When block
`B` becomes canonical, for every flagged transaction still pending that has been in the pool since
before `B` started building (`H <= B - 2`):

- if its predicates held at the start of `B` (evaluated against `B - 1`'s final state when that
  block committed) it should have been filled or killed in `B`, so it is evicted;
- if its predicates hold at the end of `B` it was satisfiable in `B` and did not land, so it is
  evicted;
- otherwise it keeps waiting, and the pool remembers whether it is satisfiable at the start of
  `B + 1`.

A transaction admitted while `B` was building (`H = B - 1`) may not have reached the builder in
time to be considered in `B`, so its first evaluated block is `B + 1`. Block-number predicates
are evaluated for the block that closed and for the next one; flashblock-index predicates count as
satisfied, since they only restrict where in a block a transaction may land.

This runs as a pool maintenance task fed by canonical state notifications, on every node. The cost
is one evaluation of each pending flagged transaction's predicates per block.

## Scenarios

Each row names the flow, what `S` conditions on, what happens in the block, and the outcome under
this contract. `C` is a competing transaction with a higher tip.

| Flow | `S` conditions on | Block | Outcome |
| --- | --- | --- | --- |
| Top-of-block CEX/DEX arb, both bid at block start | pool slots, loose bounds | `C` swaps the pool first | `S` held at block start, `C` wrote `K(S)`: killed |
| Same, `C` targets a different pool | pool slots | `C` swaps another pool | no write to `K(S)`: filled |
| Backrun of user tx `T` | `T`'s victim nonce and the pool slots | `T` lands, `S` is woken and executes next | the write that satisfied `S` is not a conflict: filled |
| Backrun race | victim nonce and pool slots | `T` lands, `C` backruns first | `S` satisfied after `T`, `C` wrote the pool: killed |
| Backrun race, `S` only conditions on the victim's nonce | victim nonce | `T` lands, `C` backruns first | `C` does not write the nonce: filled, pays gas (as today) |
| DEX/DEX arb across two pools | both pools' slots | `C` arbs one of the pools | the shared pool was written: killed |
| Liquidation after an oracle update | oracle slot and the position's slots | update lands, another liquidator takes a different position | the other liquidator does not write `K(S)`: filled |
| Liquidation, same position | oracle slot and the position's slots | another liquidator takes the same position first | position slots written: killed |
| Competitor reverts | pool slots | `C` executes first but reverts | `C` wrote nothing `S` reads: filled |
| Same sender, two transactions on one pool | pool slots on both | the first one lands | second satisfied before the first wrote: killed unless its predicates were false until then |
| `S` arrives mid-block after `C` landed in an earlier flashblock | pool slots | `C` in flashblock 1, `S` considered in flashblock 3 | journal shows `S` held at block start and `C` wrote: killed |
| `S` arrives after the block with `C` sealed | pool slots | `C` in block `B`, `S` admitted during `B + 1` | `S` is a fresh transaction in `B + 1`: filled if its predicates hold; bound `block_number` to avoid this |
| Satisfied but the block was full | any | not included in `B` | killed at block close and by every pool at `B` |
| Leadership transfer after a kill | loose predicates | old leader killed `S` in `B`, new leader builds `B + 1` | every pool that held `S` before `B` started evicts it at `B` |
| Leadership transfer, `S` forwarded during `B` | loose predicates | as above, `S` reached the new leader while `B` built | the new leader evaluates `S` at `B + 1`: it may fill there, once |

## Design decisions

**Why writes, not shared reads.** The first design killed `S` when an earlier committed validity
transaction had *read* a location in `K(S)`. That excludes transactions that read the same oracle
or victim and target different opportunities, and it ignores ordinary transactions that move the
state. Writes are what change an opportunity; reads are not.

**Why the block is the time frame.** A transaction can only be filled in the block being built,
so "immediately" has an unambiguous meaning there, and the block's transaction sequence is the
only history every replica agrees on. Earlier blocks are summarized by the pool-side rule;
anything finer than a block (when the searcher submitted, which flashblock first saw it) is
process-local and is deliberately not part of the contract. A searcher who computed against block
`B - 1` should say so with `block_number <= B`.

**Why satisfaction is reconstructed, not observed.** Deciding from the position at which the
builder happened to first evaluate `S` would make the outcome depend on arrival time within the
block, and would wrongly kill a backrun that reacted to a flashblock after an unrelated earlier
write. Replaying the journal makes the outcome a property of the block.

**Why one flag for the whole predicate set.** Every state predicate declares a location the
searcher cares about, and a write to any of them after satisfaction means the state they
conditioned on moved. A per-predicate flag (pin only the pool slots, not the victim's nonce) is
representable in the wire format later as a predicate attribute; it is not needed to make the
semantics well defined, and it would change the EIP-712 `ValidityPredicate` struct.

**Why the kill is final.** A killed transaction's predicates typically keep holding, so leaving it
in the pool lands it later, which is the wasted gas the flag exists to prevent. Submitting a new
transaction is cheap.

**Griefing.** While validity signatures are optional, anyone holding a searcher's raw transaction
can resubmit it with the flag. The same party could already alter the predicates to make the
transaction land when unintended or never; the flag adds no new capability. Signatures bind the
flag into `ValidityAuthorization`.

## Known limits

- A flagged transaction admitted after the block containing the competitor is a fresh bid: the
  journal only covers the block being built, and the pool rule only applies to blocks the
  transaction was pending for. Bound `block_number` to the block the bid was computed against.
- The pool rule gives a transaction admitted during block `B` its first evaluation at `B + 1`, so
  a failover within that window can fill a transaction the old leader killed, once.
- Writes to the sender's own nonce or balance by its earlier transactions, and to a watched
  victim's nonce by an unrelated transaction of the victim, kill a satisfied flagged transaction
  even though the opportunity may be intact. Condition on the state that matters.
- Position `0` is the state after the block's sequencer transactions; a deposit that moves a
  watched balance is not a write in the journal.
- The native builder applies kills only from a payload it returns; a pre-Denim payload can still
  be superseded after its kills applied. The flashblocks builder applies a block's kills whenever
  it finalizes the block, including a block abandoned at its deadline or superseded by a new
  forkchoice update, so a searcher can be killed on that builder by a competitor that never landed;
  the pool-side rule on other nodes does not share that kill. Validity lifetimes bound the harm.
- The pool records the admission head from the latest canonical block it has processed. A
  transaction admitted in the instant between a block committing and the pool observing it is
  recorded one block early and may be evaluated one block early.
- A flagged transaction whose predicates held but which was then rejected during execution (for
  example, by a resource limit) is reported both as rejected and, at block close, as unfilled.

## Open questions for searchers

- Which locations do your predicates read today, and would a per-predicate pin (exclude the
  victim's nonce from the kill check) change how you bid?
- How often does your bid reach the sequencer after the competitor's block has sealed? That case
  needs a `block_number` bound today.
- Is "killed when satisfiable in a block it missed" acceptable when the block was full, or would
  you rather carry the bid to the next block when nothing you read has changed?
