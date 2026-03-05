# Fault Proof

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Super Root State Transition](#super-root-state-transition)
  - [Transition State](#transition-state)
  - [Invalid State](#invalid-state)
  - [Local Safe Block Derivation](#local-safe-block-derivation)
  - [Consolidation](#consolidation)
- [Fault Proof Program State Transition](#fault-proof-program-state-transition)
- [Security Considerations](#security-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

Interop introduces the Super Fault Dispute Game as a
new [dispute game](../fault-proof/stage-one/dispute-game-interface.md) type. It is based on the
pre-interop [fault dispute game][fault-dispute-game] with some modifications:

- Claims at and above the split depth are commitments to the state within the super root state transition described
  below instead of output roots.
- The L2 block number is replaced by the timestamp of the proposed super root.
  - The `l2BlockNumber()` method has been renamed `l2SequenceNumber()` in `IDisputeGame` interface to reflect that this
    is an arbitrary identifier within the fully ordered sequence of chain states.
- The [L2 block number challenge](../fault-proof/stage-one/fault-dispute-game.md#l2-block-number-challenge) is removed.
  The super root state transition is now able to invalidate all proposals with an incorrect proposal timestamp as part
  of the state transition.
- While claims in the bottom half of the game still commit to the state of the FPVM, the fault proof program being
  executed changes to verify the disputed step of the super root state transition instead of the application of a
  disputed L2 block.
- The L2 block number preimage oracle local key now always provides the timestamp of the proposal.
- The L2 chain ID preimage oracle local key is no longer used and is never populated by the dispute game.

## Super Root State Transition

The super fault dispute game defines a state transition from the current anchor state super root to the canonical super
root at the game's proposal timestamp, if one can be derived from the available L1 data or the invalid state if not.

As the valid state transition always extends to the game proposal's timestamp, proposing a super root with a timestamp
less than or greater than game's proposal timestamp will be found to be invalid. Similarly any root claim that is the
hash of a `TransitionState` will be found to be invalid.

To reduce the amount of processing required in a single invocation of the FPVM, the state transition breaks the
transition from the super root at one timestamp to the super root at the next timestamp into 128 steps. The claims for
these intermediate steps are `keccak256` hash of a [transition state](#transition-state). These 128 steps repeat to
transition between the super root at each timestamp from the anchor state until the proposal time is reached.

### Transition State

A transition state is RLP encoded data consisting of:

- `SuperOutput []byte` - the encoded super output at the immediately prior timestamp. This is the super root that is
  being transformed _from_.
- `PendingProgress []LocalSafeBlock` - the next block derived for each chain from L1 batch data, prior to validating
  executing messages.
  - `LocalSafeBlock` is two `bytes32` RLP encoded, the first being the block hash and the second the output root for
    the block.
- `Step` a `uint64` recording the number of steps applied to this `TransitionState` since the `SuperOutput`.

### Invalid State

The invalid state is defined as `keccak256(utf8("invalid"))`. This state is used to indicate a state transition where
there is no possible valid proposal. The dispute game contract MUST prevent a game from being created where the root
claim is the invalid state.

When the prestate is the invalid state, the post state is also the invalid state.

### Local Safe Block Derivation

The first 127 steps derive the next local safe block for one chain in the super root using
the [derivation process](../protocol/derivation.md). Chains are processed in the order they appear in the super root
(ascending order of chain ID). The `Step` is incremented after each step.

For each step, the valid post state `TransitionState` is calculated by the algorithm:

- If the prestate is a SuperRoot, convert it to a `TransitionState` with `Step = 0` and `PendingProgress` an empty
  array.
- If `Step` is less than the number of chains included in `SuperOutput`, derive the next local safe block for the chain
  at
  index `Step` in the `SuperRoot` using the [derivation process](../protocol/derivation.md). Executing messages are not
  checked at this stage.
  - No block is derived if `Step` is greater than the number of chains in the super root.
  - If the derivation process is unable to derive the next L1 block because the L1 head is reached, the post state is
    the [invalid state](#invalid-state)
- Increment `Step`

Since one step is required per chain in the dependency set, the current dispute game can support a maximum of 127 chains
in the dependency set. If a greater number of chains are required in the future the number of steps can be increased.

### Consolidation

When the prestate is a `TransitionState` with `Step = 127`, the validity of executing messages is checked and any
local safe blocks found to contain invalid executing messages are replaced with deposit only blocks. This includes
recursively replacing any blocks with executing messages that became invalid because of another block being replaced.

The post state is defined as a super output where `timestamp` is the `SuperOutput` timestamp + 1, and the output roots
are set to the output roots of the validated blocks (including any required replacements).

## Fault Proof Program State Transition

Below the split depth, claims correspond to execution trace commitments of the FPVM, as with the pre-interop
[fault dispute game][fault-dispute-game]. A single **ABSOLUTE_PRESTATE** continues to be used, with the fault proof
program identifying the type of step to perform based on the agreed prestate.

## Security Considerations

TODO

[fault-dispute-game]: ../fault-proof/stage-one/fault-dispute-game.md
