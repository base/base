# Honest Challenger (Fault Dispute Game)

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Invariants](#invariants)
- [Fault Dispute Game Responses](#fault-dispute-game-responses)
  - [Moves](#moves)
  - [Steps](#steps)
  - [Timeliness](#timeliness)
- [Resolution](#resolution)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The honest challenger is an agent interacting in the [Fault Dispute Game](fault-dispute-game.md)
that supports honest claims and disputes false claims.
An honest challenger strives to ensure a correct, truthful, game resolution.
The honest challenger is also _rational_ as any deviation from its behavior will result in
negative outcomes.
This document specifies the expected behavior of an honest challenger.

The Honest Challenger has two primary duties:

1. Support valid root claims in Fault Dispute Games.
2. Dispute invalid root claims in Fault Dispute Games.

The honest challenger polls the `DisputeGameFactory` contract for new and on-going Fault
Dispute Games.
For verifying the legitimacy of claims, it relies on a synced, trusted rollup node
as well as a trace provider (ex: [Cannon](../cannon-fault-proof-vm.md)).
The trace provider must be configured with the [ABSOLUTE_PRESTATE](fault-dispute-game.md#execution-trace)
of the game being interacted with to generate the traces needed to make truthful claims.

## Invariants

To ensure an accurate and incentive compatible fault dispute system, the honest challenger behavior must preserve
three invariants for any game:

1. The game resolves as `DefenderWins` if the root claim is correct and `ChallengerWins` if the root claim is incorrect
2. The honest challenger is refunded the bond for every claim it posts and paid the bond of the parent of that claim
3. The honest challenger never counters its own claim

## Fault Dispute Game Responses

The honest challenger determines which claims to counter by iterating through the claims in the order they are stored
in the contract. This ordering ensures that a claim's ancestors are processed prior to the claim itself. For each claim,
the honest challenger determines and tracks the set of honest responses to all claims, regardless of whether that
response already exists in the full game state.

The root claim is considered to be an honest claim if and only if it has a
[state witness Hash](fault-dispute-game.md#claims) that agrees with the honest challenger's state witness hash for the
root claim.

The honest challenger should counter a claim if and only if:

1. The claim is a child of a claim in the set of honest responses
2. The set of honest responses, contains a sibling to the claim with a trace index greater than or equal to the
   claim's trace index

Note that this implies the honest challenger never counters its own claim, since there is at most one honest counter to
each claim, so an honest claim never has an honest sibling.

### Moves

To respond to a claim with a depth in the range of `[1, MAX_DEPTH]`, the honest challenger determines if the claim
has a valid commitment. If the state witness hash matches the honest challenger's at the same trace
index, then we disagree with the claim's stance by move to [defend](fault-dispute-game.md#defend).
Otherwise, the claim is [attacked](fault-dispute-game.md#attack).

The claim that would be added as a result of the move is added to the set of honest moves being tracked.

If the resulting claim does not already exist in the full game state, the challenger issue the move by calling
the `FaultDisputeGame` contract.

### Steps

At the max depth of the game, claims represent commitments to the state of the fault proof VM
at a single instruction step interval.
Because the game can no longer bisect further, when the honest challenger counters these claims,
the only option for an honest challenger is to execute a VM step on-chain to disprove the claim at `MAX_GAME_DEPTH`.

If the `counteredBy` of the claim being countered is non-zero, the claim has already been countered and the honest
challenger does not perform any action.

Otherwise, similar to the above section, the honest challenger will issue an
[attack step](fault-dispute-game.md#step-types) when in response to such claims with
invalid state witness commitments. Otherwise, it issues a _defense step_.

### Timeliness

The honest challenger responds to claims as soon as possible to avoid the clock of its
counter-claim from expiring.

## Resolution

When the [chess clock](fault-dispute-game.md#game-clock) of a
[subgame root](fault-dispute-game.md#resolution) has run out, the subgame can be resolved.
The honest challenger should resolve all subgames in bottom-up order, until the subgame
rooted at the game root is resolved.

The honest challenger accomplishes this by calling the `resolveClaim` function on the
`FaultDisputeGame` contract. Once the root claim's subgame is resolved,
the challenger then finally calls the `resolve` function to resolve the entire game.

The `FaultDisputeGame` does not put a time cap on resolution - because of the liveness
assumption on honest challengers and the bonds attached to the claims they’ve countered,
challengers are economically incentivized to resolve the game promptly to capture the bonds.
