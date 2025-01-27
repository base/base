# Holocene L2 Chain Derivation Changes

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Holocene Derivation](#holocene-derivation)
  - [Summary](#summary)
  - [Frame Queue](#frame-queue)
  - [Channel Bank](#channel-bank)
    - [Pruning](#pruning)
    - [Timeout](#timeout)
    - [Reading & Frame Loading](#reading--frame-loading)
  - [Span Batches](#span-batches)
  - [Batch Queue](#batch-queue)
    - [Fast Channel Invalidation](#fast-channel-invalidation)
  - [Engine Queue](#engine-queue)
  - [Attributes Builder](#attributes-builder)
  - [Activation](#activation)
- [Rationale](#rationale)
  - [Strict Frame and Batch Ordering](#strict-frame-and-batch-ordering)
  - [Partial Span Batch Validity](#partial-span-batch-validity)
  - [Fast Channel Invalidation](#fast-channel-invalidation-1)
  - [Steady Block Derivation](#steady-block-derivation)
  - [Less Defensive Protocol](#less-defensive-protocol)
- [Security and Implementation Considerations](#security-and-implementation-considerations)
  - [Reorgs](#reorgs)
  - [Batcher Hardening](#batcher-hardening)
  - [Sync Start](#sync-start)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

# Holocene Derivation

## Summary

The Holocene hardfork introduces several changes to block derivation rules that render the
derivation pipeline mostly stricter and simpler, improve worst-case scenarios for Fault Proofs and
Interop. The changes are:

- _Strict Batch Ordering_ required batches within and across channels to be strictly ordered.
- _Partial Span Batch Validity_ determines the validity of singular batches from a span batch
individually, only invalidating the remaining span batch upon the first invalid singular batch.
- _Fast Channel Invalidation_, similarly to Partial Span Batch Validity applied to the channel
layer, forward-invalidates a channel upon finding an invalid batch.
- _Steady Block Derivation_ derives invalid payload attributes immediately as deposit-only
blocks.

The combined effect of these changes is that the impact of an invalid batch is contained to the
block number at hand, instead of propagating forwards or backwards in the safe chain, while also
containing invalid payloads at the engine stage to the engine, not propagating backwards in the
derivation pipeline.

Holocene derivation comprises the following changes to the derivation pipeline to achieve the above.

## Frame Queue

The frame queue retains its function and queues all frames of the last batcher transaction(s) that
weren't assembled into a channel yet. Holocene still allows multiple frames per batcher transaction,
possibly from different channels. As before, this allows for optionally filling up the remaining
space of a batcher transaction with a starting frame of the next channel.

However, Strict Batch Ordering leads to the following additional checks and rules to the frame
queue:

- If a _non-first frame_ (i.e., a frame with index >0) decoded from a batcher transaction is _out of
order_, it is **immediately dropped**, where the frame is called _out of order_ if
  - its frame number is not the previous frame's plus one, if it has the same channel ID, or
  - the previous frame already closed the channel with the same ID, or
  - the non-first frame has a different channel ID than the previous frame in the frame queue.
- If a _first frame_ is decoded while the previous frame isn't a _last frame_ (i.e., `is_last` is
`false`), all previous frames for the same channel are dropped and this new first frame remains in
the queue.

These rules guarantee that the frame queue always holds frames whose indices are ordered,
contiguous and include the first frame, per channel. Plus, a first frame of a channel is either the
first frame in the queue, or is preceded by a closing frame of a previous channel.

Note that these rules are in contrast to pre-Holocene rules, where out of order frames were
buffered. Pre-Holocene, frame validity checks were only done at the Channel Bank stage. Performing
these checks already at the Frame Queue stage leads to faster discarding of invalid frames, keeping
the memory consumption of any implementation leaner.

## Channel Bank

Because channel frames have to arrive in order, the Channel Bank becomes much simpler and only
holds at most a single channel at a time.

### Pruning

Pruning is vastly simplified as there is at most only one open channel in the channel bank. So the
channel bank's queue becomes effectively a staging slot for a single channel, the _staging channel_.
The `MAX_CHANNEL_BANK_SIZE` parameter is no longer used, and the compressed size of the staging
channel is required to be at most `MAX_RLP_BYTES_PER_CHANNEL` (else the channel is dropped). Note this
latter rule is both a distinct condition and distinct effect, compared to the existing rule
that the _uncompressed_ size of any given channel is _clipped_ to `MAX_RLP_BYTES_PER_CHANNEL` [during decompression](../derivation.md#channel-format).

### Timeout

The timeout is applied as before, just only to the single staging channel.

### Reading & Frame Loading

The frame queue is guaranteed to hold ordered and contiguous frames, per channel. So reading and
frame loading becomes simpler in the channel bank:

- A first frame for a new channel starts a new channel as the staging channel.
  - If there already is an open, non-completed staging channel, it is dropped and replaced by this
  new channel. This is consistent with how the frame queue drops all frames of a non-closed channel
  upon the arrival of a first frame for a new channel.
- If the current channel is timed-out, but not yet pruned, and the incoming frame would be the next
correct frame for this channel, the frame and channel are dropped, including all future frames for
the channel that might still be in the frame queue. Note that the equivalent rule was already
present pre-Holocene.
- After adding a frame to the staging channel, the channel is dropped if its raw compressed size as
defined in the Bedrock specification is larger than `MAX_RLP_BYTES_PER_CHANNEL`. This rule replaces
the total limit of all channels' combined sizes by `MAX_CHANNEL_BANK_SIZE` before Holocene.

## Span Batches

Partial Span Batch Validity changes the atomic validity model of [Span Batches](../delta/span-batches.md).
In Holocene, a span batch is treated as an optional stage in the derivation pipeline that sits
before the batch queue, so that the batch queue pulls singular batches from this previous Span Batch
stage. When encountering an invalid singular batch, it is dropped, as is the remaining span batch
for consistency reasons. We call this _forwards-invalidation_. However, we don't
_backwards-invalidate_ previous valid batches that came from the same span batch, as pre-Holocene.

When a batch derived from the current staging channel is a singular batch, it is directly forwarded
to the batch queue. Otherwise, it is set as the current span batch in the span batch stage. The
following span batch validity checks are done, before singular batches are derived from it.
Definitions are borrowed from the [original Span Batch specs](../delta/span-batches.md).

- If the span batch _L1 origin check_ is not part of the canonical L1 chain, the span batch is
invalid.
- A failed parent check invalidates the span batch.
- If `span_start.timestamp > next_timestamp`, the span batch is invalid, because we disallow gaps
due to the new strict batch ordering rules.
- If `span_end.timestamp < next_timestamp`, the span batch is set to have `past` validity, as it
doesn't contain any new batches (this would also happen if applying timestamp checks to each derived
singular batch individually). See below in the [Batch Queue](#batch-queue) section about the new
`past` validity.
- Note that we still allow span batches to overlap with the safe chain (`span_start.timestamp <
next_timestamp`).

If any of the above checks invalidate the span batch, it is `drop`ped and the remaining channel from
which the span batch was derived, is also immediately dropped (see also [Fast Channel
Invalidation](#fast-channel-invalidation)). However, a `past` span batch is only dropped, without
dropping the remaining channel.

## Batch Queue

The batch queue is also simplified in that batches are required to arrive strictly ordered, and any
batches that violate the ordering requirements are immediately dropped, instead of buffered.

So the following changes are made to the [Bedrock Batch Queue](../derivation.md#batch-queue):

- The reordering step is removed, so that later checks will drop batches that are not sequential.
- The `future` batch validity status is removed, and batches that were determined to be in the
future are now directly `drop`-ped. This effectively disallows gaps, instead of buffering future
batches.
- A new batch validity `past` is introduced. A batch has `past` validity if its timestamp is before
or equal to the safe head's timestamp. This also applies to span batches.
- The other rules stay the same, including empty batch generation when the sequencing window
elapses.

Note that these changes to batch validity rules also activate by the L1 inclusion block timestamp of
a batch, not with the batch timestamp. This is important to guarantee consistent validation rules
for the first channel after Holocene activation.

The `drop` and `past` batch validities cause the following new behavior:

- If a batch is found to be invalid and is dropped, the remaining span batch it originated from, if
applicable, is also discarded.
- If a batch is found to be from the `past`, it is silently dropped and the remaining span batch
continues to be processed. This applies to both, span and singular batches.

Note that when the L1 origin of the batch queue moves forward, it is guaranteed that it is empty,
because future batches aren't buffered any more. Furthermore, because future batches are directly
dropped, the batch queue effectively becomes a simpler _batch stage_ that holds at most one span
batch from which singular batches are read from, and doesn't buffer singular batches itself in a
queue any more. A valid batch is directly forwarded to the next stage.

### Fast Channel Invalidation

Furthermore, upon finding an invalid batch, the remaining channel it got derived from is also discarded.

## Engine Queue

If the engine returns an `INVALID` status for a regularly derived payload, the payload is replaced
by a payload with the same fields, except for the `transaction_list`, which is trimmed to include
only its deposit transactions.

As before, a failure to then process the deposit-only attributes is a critical error.

If an invalid payload is replaced by a deposit-only payload, for consistency reasons, the remaining
span batch, if applicable, and channel it originated from are dropped as well.

## Attributes Builder

Starting after the fork activation block, the `PayloadAttributes` produced by the attributes builder will include
the `eip1559Params` field described in the [execution engine specs](./exec-engine.md#eip1559params-encoding). This
value exists within the `SystemConfig`.

On the fork activation block, the attributes builder will include a 0'd out `eip1559Params`, as to instruct
the engine to use the [canyon base fee parameter constants](../exec-engine.md#1559-parameters). This
is to prime the pipeline's view of the `SystemConfig` with the default EIP-1559 parameter values. After the first
Holocene payload has been processed, future payloads should use the `SystemConfig`'s EIP-1559 denominator and elasticity
parameter as the `eip1559Params` field's value. When the pipeline encounters a `UpdateType.EIP_1559_PARAMS`,
`ConfigUpdate` event, the pipeline's system config will be synchronized with the `SystemConfig` contract's.

## Activation

The new batch rules activate when the _L1 inclusion block timestamp_ is greater or equal to the
Holocene activation timestamp. Note that this is in contrast to how span batches activated in
[Delta](../delta/overview.md), namely via the span batch L1 origin timestamp.

When the L1 traversal stage of the derivation pipeline moves its origin to the L1 block whose
timestamp is the first to be greater or equal to the Holocene activation timestamp, the derivation
pipeline's state is mostly reset by **discarding**

- all frames in the frame queue,
- channels in the channel bank, and
- all batches in the batch queue.

The three stages are then replaced by the new Holocene frame queue, channel bank and batch queue
(and, depending on the implementation, the optional span batch stage is added).

Note that batcher implementations must be aware of this activation behavior, so any frames of a
partially submitted channel that were included pre-Holocene must be sent again. This is a very
unlikely scenario since production batchers are usually configured to submit a channel in a single
transaction.

# Rationale

## Strict Frame and Batch Ordering

Strict Frame and Batch Ordering simplifies implementations of the derivation pipeline, and leads to
better worst-case cached data usage.

- The frame queue only ever holds frames from a single batcher transaction.
- The channel bank only ever holds a single staging channel, that is either being built up by
incoming frames, or is is being processed by later stages.
- The batch queue only ever holds at most a single span batch (that is being processed) and a single singular
batch (from the span batch, or the staging channel directly)
- The sync start greatly simplifies in the average production case.

This has advantages for Fault Proof program implementations.

## Partial Span Batch Validity

Partial Span Batch Validity guarantees that a valid singular batch derived from a span batch can
immediately be processed as valid and advance the safe chain, instead of being in an undecided state
until the full span batch is converted into singular batches. This leads to swifter derivation and
gives strong worst-case guarantees for Fault Proofs because the validity of a block doesn't depend
on the validity of any future blocks any more. Note that before Holocene, to verify the first block
of a span batch required validating the full span batch.

## Fast Channel Invalidation

The new Fast Channel Invalidation rule is a consistency implication of the Strict Ordering Rules.
Because batches inside channels must be ordered and contiguous, assuming that all batches inside a
channel are self-consistent (i.e., parent L2 hashes point to the block resulting from the previous
batch), an invalid batch also forward-invalidates all remaining batches of the same channel.

## Steady Block Derivation

Steady Block Derivation changes the derivation rules for invalid payload attributes, replacing an
invalid payload by a deposit-only/empty payload. Crucially, this means that the effect of an invalid
payload doesn't propagate backwards in the derivation pipeline. This has benefits for Fault Proofs
and Interop, because it guarantees that batch validity is not influenced by future stages and the
block derived from a valid batch will be determined by the engine stage before it pulls new payload
attributes from the previous stage. This avoids larger derivation pipeline resets.

## Less Defensive Protocol

The stricter derivation rules lead to a less defensive protocol. The old protocol rules allowed for
second chances for invalid payloads and submitting frames and batches within channels out of order.
Experiences from running OP Stack chains for over one and a half years have shown that these relaxed
derivation rules are (almost) never needed, so stricter rules that improve worst-case scenarios for
Fault Proofs and Interop are favorable.

Furthermore, the more relaxed rules created a lot more corner cases and complex interactions, which
made it harder to reason about and test the protocol, increasing the risk of chain splits between
different implementations.

# Security and Implementation Considerations

## Reorgs

Before Steady Block Derivation, invalid payloads got second chances to be replaced by valid future
payloads. Because they will now be immediately replaced by as deposit-only payloads, there is a
theoretical heightened risk for unsafe chain reorgs. To the best of our knowledge, we haven't
experienced this on OP Mainnet or other mainnet OP Stack chains yet.

The only conceivable scenarios in which a _valid_ batch leads to an _invalid_ payload are

- a buggy or malicious sequencer+batcher
- in the future, that an previously valid Interop dependency referenced in that payload is later
invalidated, while the block that contained the Interop dependency got already batched.

It is this latter case that inspired the Steady Block Derivation rule. It guarantees that the
secondary effects of an invalid Interop dependency are contained to a single block only, which
avoids a cascade of cross-L2 Interop reorgs that revisit L2 chains more than once.

## Batcher Hardening

In a sense, Holocene shifts some complexity from derivation to the batching phase. Simpler and
stricter derivation rules need to be met by a more complex batcher implementation.

The batcher must be hardened to guarantee the strict ordering requirements. They are already mostly
met in practice by the current Go implementation, but more by accident than by design. There are
edge cases in which the batcher might violate the strict ordering rules. For example, if a channel
fails to submit within a set period, the blocks are requeued and some out of order batching might
occur. A batcher implementation also needs to take extra care that dynamic blobs/calldata switching
doesn't lead to out of order or gaps of batches in scenarios where blocks are requeued, while future
channels are already waiting in the mempool for inclusion.

Batcher implementations are suggested to follow a fixed nonce to block-range assignment, once the
first batcher transaction (which is almost always the only batcher transaction for a channel for
current production batcher configurations) starts being submitted. This should avoid out-of-order or
gaps of batches. It might require to implement some form of persistence in the transaction
management, since it isn't possible to reliably recover all globally pending batcher transactions in
the L1 network.

Furthermore, batcher implementations need to be made aware of the Steady Block Derivation rules,
namely that invalid payloads will be derived as deposit-only blocks. So in case of an unsafe reorg,
the batcher should wait on the sequencer until it has derived all blocks from L1 in order to only
start batching new blocks on top of the possibly deposit-only derived reorg'd chain segment. The
sync-status should repeatedly be queried and matched against the expected safe chain. In case of any
discrepancy, the batcher should then stop batching and wait for the sequencer to fully derive up
until the latest L1 batcher transactions, and only then continue batching.

## Sync Start

Thanks to the new strict frame and batch ordering rules, the sync start algorithm can be simplified
in the average case. The rules guarantee that

- an incoming first frame for a new channel leads to discarding previous incomplete frames for a
non-closed previous channel in the frame queue and channel bank, and
- when the derivation pipeline L1 origin progresses, the batch queue is empty.

So the sync start algorithm can optimistically select the last L2 unsafe, safe and finalized heads
from the engine and if the L2 safe head's L1 origin is _plausible_ (see the
[original sync start description](../derivation.md#finding-the-sync-starting-point) for details),
start deriving from this L1 origin.

- If the first frame we find is a _first frame_ for a channel that includes the safe head (TBD: or
even just the following L2 block with the current safe head as parent), we can
safely continue derivation from this channel because no previous derivation pipeline state could
have influenced the L2 safe head.
- If the first frame we find is a non-first frame, then we need to walk back a full channel
timeout window to see if we find the start of that channel.
  - If we find the starting frame, we can continue derivation from it.
  - If we don't find the starting frame, we need to go back a full channel timeout window before the
    finalized L2 head's L1 origin.

Note regarding the last case that if we don't find a starting frame within a channel timeout window,
the channel we did find a frame from must be timed out and would be discarded. The safe block we're
looking for can't be in any channel that timed out before its L1 origin so we wouldn't need to
search any further back, so we go back a channel timeout before the finalized L2 head.
