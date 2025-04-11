# AnchorStateRegistry

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Dispute Game](#dispute-game)
  - [Respected Game Type](#respected-game-type)
  - [Dispute Game Finality Delay (Airgap)](#dispute-game-finality-delay-airgap)
  - [Registered Game](#registered-game)
  - [Respected Game](#respected-game)
  - [Blacklisted Game](#blacklisted-game)
  - [Retirement Timestamp](#retirement-timestamp)
  - [Retired Game](#retired-game)
  - [Proper Game](#proper-game)
  - [Resolved Game](#resolved-game)
  - [Finalized Game](#finalized-game)
  - [Valid Claim](#valid-claim)
  - [Truly Valid Claim](#truly-valid-claim)
  - [Starting Anchor State](#starting-anchor-state)
  - [Anchor Game](#anchor-game)
  - [Anchor Root](#anchor-root)
- [Assumptions](#assumptions)
  - [aASR-001: Dispute Game contracts properly report important properties](#aasr-001-dispute-game-contracts-properly-report-important-properties)
    - [Mitigations](#mitigations)
  - [aASR-002: DisputeGameFactory properly reports its created games](#aasr-002-disputegamefactory-properly-reports-its-created-games)
    - [Mitigations](#mitigations-1)
  - [aASR-003: Incorrectly resolving games will be invalidated before they have Valid Claims](#aasr-003-incorrectly-resolving-games-will-be-invalidated-before-they-have-valid-claims)
    - [Mitigations](#mitigations-2)
- [Invariants](#invariants)
  - [iASR-001: Games are represented as Proper Games accurately](#iasr-001-games-are-represented-as-proper-games-accurately)
    - [Impact](#impact)
    - [Dependencies](#dependencies)
  - [iASR-002: All Valid Claims are Truly Valid Claims](#iasr-002-all-valid-claims-are-truly-valid-claims)
    - [Impact](#impact-1)
    - [Dependencies](#dependencies-1)
  - [iASR-003: The Anchor Game is a Truly Valid Claim](#iasr-003-the-anchor-game-is-a-truly-valid-claim)
    - [Impact](#impact-2)
    - [Dependencies](#dependencies-2)
  - [iASR-004: Invalidation functions operate correctly](#iasr-004-invalidation-functions-operate-correctly)
    - [Impact](#impact-3)
    - [Dependencies](#dependencies-3)
  - [iASR-005: The Anchor Game is recent enough to be fault provable](#iasr-005-the-anchor-game-is-recent-enough-to-be-fault-provable)
    - [Impact](#impact-4)
- [Function Specification](#function-specification)
  - [constructor](#constructor)
  - [initialize](#initialize)
  - [paused](#paused)
  - [respectedGameType](#respectedgametype)
  - [retirementTimestamp](#retirementtimestamp)
  - [disputeGameFinalityDelaySeconds](#disputegamefinalitydelayseconds)
  - [setRespectedGameType](#setrespectedgametype)
  - [updateRetirementTimestamp](#updateretirementtimestamp)
  - [blacklistDisputeGame](#blacklistdisputegame)
  - [isGameRegistered](#isgameregistered)
  - [isGameRespected](#isgamerespected)
  - [isGameBlacklisted](#isgameblacklisted)
  - [isGameRetired](#isgameretired)
  - [isGameProper](#isgameproper)
  - [isGameResolved](#isgameresolved)
  - [isGameFinalized](#isgamefinalized)
  - [isGameClaimValid](#isgameclaimvalid)
  - [getAnchorRoot](#getanchorroot)
  - [anchors](#anchors)
  - [setAnchorState](#setanchorstate)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `AnchorStateRegistry` was designed as a registry where `DisputeGame` contracts could store and
register their results so that these results could be used as the starting states for new
`DisputeGame` instances. These starting states, called "anchor states", allow new `DisputeGame`
contracts to use a newer starting state to bound the size of the execution trace for any given
game.

We are generally aiming to shift the `AnchorStateRegistry` to act as a unified source of truth for
the validity of `DisputeGame` contracts and their corresponding root claims. This specification
corresponds to the first iteration of the `AnchorStateRegistry` that will move us in this
direction.

## Definitions

### Dispute Game

> See [Fault Dispute Game](fault-dispute-game.md)

A Dispute Game is a smart contract that makes a determination about the validity of some claim. In
the context of the OP Stack, the claim is generally assumed to be a claim about the value of an
output root at a given L2 block height. We assume that all Dispute Game contracts using the same
AnchorStateRegistry contract are arguing over the same underlying state/claim structure.

### Respected Game Type

The `AnchorStateRegistry` contract defines a **Respected Game Type** which is the Dispute Game type
that is considered to be the correct by the `AnchorStateRegistry` and, by extension, other
contracts that may rely on the assertions made within the `AnchorStateRegistry`. The Respected Game
Type is, in a more general sense, a game type that the system believes will resolve correctly. For
now, the `AnchorStateRegistry` only allows a single Respected Game Type.

### Dispute Game Finality Delay (Airgap)

The **Dispute Game Finality Delay** or **Airgap** is the amount of time that must elapse after a
game resolves before the game's result is considered "final".

### Registered Game

A Dispute Game is considered to be a **Registered Game** if the game contract was created by the
system's `DisputeGameFactory` contract.

### Respected Game

A Dispute Game is considered to be a **Respected Game** if the game contract's game type **was**
the Respected Game Type defined by the `AnchorStateRegistry` contract at the time of the game's
creation. Games that are not Respected Games cannot be used as an Anchor Game. See
[Respected Game Type](#respected-game-type) for more information.

### Blacklisted Game

A Dispute Game is considered to be a **Blacklisted Game** if the game contract's address is marked
as blacklisted inside of the `AnchorStateRegistry` contract.

### Retirement Timestamp

The **Retirement Timestamp** is a timestamp value maintained within the `AnchorStateRegistry` that
can be used to invalidate games. Games with a creation timestamp less than or equal to the
Retirement Timestamp are automatically considered to be invalid.

The RetirementTimestamp has the effect of retiring all games created before the specific
transaction in which the retirement timestamp was set. This includes all games created in the same
block as the transaction that set the Retirement Timestamp. We acknowledge the edge-case that games
created in the same block *after* the Retirement Timestamp was set will be considered Retired Games
even though they were technically created "after" the Retirement Timestamp was set.

### Retired Game

A Dispute Game is considered to be a **Retired Game** if the game contract was created with a
timestamp less than or equal to the [Retirement Timestamp](#retirement-timestamp).

### Proper Game

A Dispute Game is considered to be a **Proper Game** if it has not been invalidated through any of
the mechanisms defined by the `AnchorStateRegistry` contract. A Proper Game is, in a sense, a
"clean" game that exists in the set of games that are playing out correctly in a bug-free manner. A
Dispute Game can be a Proper Game even if it has not yet resolved or resolves in favor of the
Challenger.

A Dispute Game that is **NOT** a Proper Game can also be referred to as an **Improper Game** for
brevity. A Dispute Game can go from being a Proper Game to later *not* being an **Improper Game**
if it is invalidated by being [blacklisted](#blacklisted-game) or [retired](#retired-game).

**ALL** Dispute Games **TEMPORARILY** become Improper Games while the
[Pause Mechanism](../../protocol/stage-1.md#pause-mechanism) is active. However, this is
a *temporary* condition such that Registered Games that are not invalidated by
[blacklisting](#blacklisted-game) or [retirement](#retired-game) will become Proper Games again
once the pause is lifted. The Pause Mechanism is therefore a way to *temporarily* prevent Dispute
Games from being used by consumers like the `OptimismPortal` while relevant parties coordinate the
use of some other invalidation mechanism.

A Game is considered to be a Proper Game if all of the following are true:

- The game is a [Registered Game](#registered-game)
- The game is **NOT** a [Blacklisted Game](#blacklisted-game)
- The game is **NOT** a [Retired Game](#retired-game)
- The [Pause Mechanism](../../protocol/stage-1.md#pause-mechanism) is not active

### Resolved Game

A Dispute Game is considered to be a **Resolved Game** if the game has resolved a result in favor
of either the Challenger or the Defender.

### Finalized Game

A Dispute Game is considered to be a **Finalized Game** if all of the following are true:

- The game is a [Resolved Game](#resolved-game)
- The game resolved a result more than
  [Dispute Game Finality Delay](#dispute-game-finality-delay-airgap) seconds ago as defined by the
  `disputeGameFinalityDelaySeconds` variable in the `AnchorStateRegistry` contract.

### Valid Claim

A Dispute Game is considered to have a **Valid Claim** if all of the following are true:

- The game is a [Proper Game](#proper-game)
- The game is a [Respected Game](#respected-game)
- The game is a [Finalized Game](#finalized-game)
- The game resolved in favor of the root claim (i.e., in favor of the Defender)

### Truly Valid Claim

A Truly Valid Claim is a claim that accurately represents the correct root for the L2 block height
on the L2 system as would be reported by a perfect oracle for the L2 system state.

### Starting Anchor State

The Starting Anchor State is the anchor state (root and L2 block height) that is used as the
starting state for new Dispute Game instances when there is no current Anchor Game. The Starting
Anchor State is set during the initialization of the `AnchorStateRegistry` contract.

### Anchor Game

The Anchor Game is a game whose claim is used as the starting state for new Dispute Game instances.
A Game can become the Anchor Game if it has a Valid Claim and the claim's L2 block height is
greater than the claim of the current Anchor Game. If there is no current Anchor Game, a Game can
become the Anchor Game if it has a Valid Claim and the claim's L2 block height is greater than the
current Starting Anchor State's L2 block height.

After a Game becomes the Anchor Game, it will remain the Anchor Game until it is replaced by some
other Game. A Game that is retired after becoming the Anchor Game will remain the Anchor Game.

### Anchor Root

The Anchor Root is the root and L2 block height that is used as the starting state for new Dispute
Game instances. The value of the Anchor Root is the Starting Anchor State if no Anchor Game has
been set. Otherwise, the value of the Anchor Root is the root and L2 block height of the current
Anchor Game.

## Assumptions

> **NOTE:** Assumptions are utilized by specific invariants and do not apply globally. Invariants
> typically only rely on a subset of the following assumptions. Different invariants may rely on
> different assumptions. Refer to individual invariants for their dependencies.

### aASR-001: Dispute Game contracts properly report important properties

We assume that the `FaultDisputeGame` and `PermissionedDisputeGame` contracts properly and
faithfully report the following properties:

- Game type
- L2 block number
- Root claim value
- Game extra data
- Creation timestamp
- Resolution timestamp
- Resolution result
- Whether the game was the respected game type at creation

We also specifically assume that the game creation timestamp and the resolution timestamp are not
set to values in the future.

#### Mitigations

- Existing audit on the `FaultDisputeGame` contract
- Integration testing

### aASR-002: DisputeGameFactory properly reports its created games

We assume that the `DisputeGameFactory` contract properly and faithfully reports the games it has
created.

#### Mitigations

- Existing audit on the `DisputeGameFactory` contract
- Integration testing

### aASR-003: Incorrectly resolving games will be invalidated before they have Valid Claims

We assume that any games that are resolved incorrectly will be invalidated either by
[blacklisting](#blacklisted-game) or by [retirement](#retired-game) BEFORE they are considered to
have [Valid Claims](#valid-claim).

Proper Games that resolve in favor the Defender will be considered to have Valid Claims after the
[Dispute Game Finality Delay](#dispute-game-finality-delay-airgap) has elapsed UNLESS the
Pause Mechanism is active. Therefore, in the absence of the Pause Mechanism, parties responsible
for game invalidation have exactly the Dispute Game Finality Delay to invalidate a withdrawal after
it resolves incorrectly. If the Pause Mechanism is active, then any incorrectly resolving games
must be invalidated before the pause is deactivated.

#### Mitigations

- Stakeholder incentives / processes
- Incident response plan
- Monitoring

## Invariants

### iASR-001: Games are represented as Proper Games accurately

When asked if a game is a Proper Game, the `AnchorStateRegistry` must serve a response that is
identical to the response that would be given by a perfect oracle for this query.

#### Impact

**Severity: High**

If this invariant is broken, the Anchor Game could be set to an incorrect value, which would cause
future Dispute Game instances to use an incorrect starting state. This would lead games to resolve
incorrectly. Additionally, this could cause a `FaultDisputeGame` to incorrectly choose the wrong
bond refunding mode.

#### Dependencies

- [aASR-001](#aasr-001-dispute-game-contracts-properly-report-important-properties)
- [aASR-002](#aasr-002-disputegamefactory-properly-reports-its-created-games)
- [aASR-003](#aasr-003-incorrectly-resolving-games-will-be-invalidated-before-they-have-valid-claims)

### iASR-002: All Valid Claims are Truly Valid Claims

When asked if a game has a Valid Claim, the `AnchorStateRegistry` must serve a response that is
identical to the response that would be given by a perfect oracle for this query. However, it is
important to note that we do NOT say that all Truly Valid Claims are Valid Claims. It is possible
that a game has a Truly Valid Claim but the `AnchorStateRegistry` reports that the claim is not
a Valid Claim. This permits the `AnchorStateRegistry` and system-wide safety net actions to err on
the side of caution.

In a nutshell, the set of Valid Claims is a subset of the set of Truly Valid Claims.

#### Impact

**Severity: Critical**

If this invariant is broken, then any component that relies on the correctness of this function may
allow actions to occur based on invalid dispute games.

Some examples of strong negative impact are:

- Invalid Dispute Game could be used as the Anchor Game, which would cause future Dispute Game
  instances to use an incorrect starting state. This would lead these games to resolve incorrectly.
  **(HIGH)**
- Invalid Dispute Game could be used to prove or finalize withdrawals within the `OptimismPortal`
  contract. This would lead to a critical vulnerability in the bridging system. **(CRITICAL)**

#### Dependencies

- [aASR-001](#aasr-001-dispute-game-contracts-properly-report-important-properties)
- [aASR-002](#aasr-002-disputegamefactory-properly-reports-its-created-games)
- [aASR-003](#aasr-003-incorrectly-resolving-games-will-be-invalidated-before-they-have-valid-claims)

### iASR-003: The Anchor Game is a Truly Valid Claim

We require that the Anchor Game is a Truly Valid Claim. This makes it possible to use the Anchor
Game as the starting state for new Dispute Game instances. Notably, given the allowance that not
all Truly Valid Claims are Valid Claims, this invariant does not imply that the Anchor Game is a
Valid Claim.

We allow retired games to be used as the Anchor Game because the retirement mechanism is broad in a
way that commonly causes Truly Valid Claims to no longer be considered Valid Claims. We allow both
blacklisted games and retired games to remain the Anchor Game if they are already the Anchor Game.
This is because we assume games that become the Anchor Game would be invalidated *before* becoming
the Anchor Game. After the game becomes the Anchor Game, it would be possible to use that game to
execute withdrawals from the system, which would already be a critical bug in the system.

#### Impact

**Severity: High**

If this invariant is broken, an invalid Anchor Game could be used as the starting state for new
Dispute Game instances. This would lead games to resolve incorrectly.

#### Dependencies

- [aASR-001](#aasr-001-dispute-game-contracts-properly-report-important-properties)
- [aASR-002](#aasr-002-disputegamefactory-properly-reports-its-created-games)
- [aASR-003](#aasr-003-incorrectly-resolving-games-will-be-invalidated-before-they-have-valid-claims)

### iASR-004: Invalidation functions operate correctly

We require that the blacklisting and retirement functions operate correctly. Games that are
blacklisted must not be used as the Anchor Game, must not be considered Valid Games, and must not
be usable to prove or finalize withdrawals. Any game created before a transaction that updates the
retirement timestamp must not be set as the Anchor Game, must not be considered Valid Games, and
must not be usable to prove or finalize withdrawals.

#### Impact

**Severity: High/Critical**

If this invariant is broken, the Anchor Game could be set to an incorrect value, which would cause
future Dispute Game instances to use an incorrect starting state. This would lead games to resolve
incorrectly and would be considered a High Severity issue. Issues that would allow users to
finalize withdrawals with invalidated games would be considered Critical Severity.

#### Dependencies

- [aASR-003](#aasr-003-incorrectly-resolving-games-will-be-invalidated-before-they-have-valid-claims)

### iASR-005: The Anchor Game is recent enough to be fault provable

We require that the Anchor Game corresponds to an L2 block with an L1 origin timestamp that is no
older than 6 months from the current timestamp. This time constraint is necessary because the fault
proof VM must walk backwards through L1 blocks to verify derivation, and processing 7 months worth
of L1 blocks approaches the maximum time available to challengers in the dispute game process.

#### Impact

**Severity: High**

If this invariant is broken, challengers will be unable to participate in fault proofs within the
allotted response time, and resolution would require intervention from the Proxy Admin Owner.

## Function Specification

### constructor

- MUST set the value of the [Dispute Game Finality Delay](#dispute-game-finality-delay-airgap).

### initialize

- MUST only be triggerable once.
- MUST set the value of the `SystemConfig` contract that stores the address of the Guardian.
- MUST set the value of the `DisputeGameFactory` contract that creates Dispute Game instances.
- MUST set the value of the [Starting Anchor State](#starting-anchor-state).
- MUST set the value of the initial [Respected Game Type](#respected-game-type).
- MUST set the value of the [Retirement Timestamp](#retirement-timestamp) to the current block
  timestamp. NOTE that this is a safety mechanism that invalidates all existing Dispute Game
  contracts to support the safe transition away from the `OptimismPortal` as the source of truth
  for game validity. In this way, the `AnchorStateRegistry` does not need to consider the state of
  the legacy blacklisting/retirement mechanisms within the `OptimismPortal` and starts from a clean
  slate.

### paused

Returns the value of `paused()` from the `SystemConfig` contract.

### respectedGameType

Returns the value of the currently [Respected Game Type](#respected-game-type).

### retirementTimestamp

Returns the value of the current [Retirement Timestamp](#retirement-timestamp).

### disputeGameFinalityDelaySeconds

Returns the value of the [Dispute Game Finality Delay](#dispute-game-finality-delay-airgap).

### setRespectedGameType

Permits the Guardian role to set the [Respected Game Type](#respected-game-type).

- MUST revert if called by any address other than the Guardian.
- MUST update the respected game type with the provided type.
- MUST emit an event showing that the game type was updated.

### updateRetirementTimestamp

Permits the Guardian role to update the [Retirement Timestamp](#retirement-timestamp).

- MUST revert if called by any address other than the Guardian.
- MUST set the retirement timestamp to the current block timestamp.
- MUST emit an event showing that the retirement timestamp was updated.

### blacklistDisputeGame

Permits the Guardian role to [blacklist](#blacklisted-game) a Dispute Game.

- MUST revert if called by any address other than the Guardian.
- MUST mark the game as blacklisted.
- MUST emit an event showing that the game was blacklisted.

### isGameRegistered

Determines if a game is a Registered Game.

- MUST return `true` if and only if the game was created by the system's `DisputeGameFactory` contract.

### isGameRespected

Determines if a game is a Respected Game.

- MUST return `true` if and only if the game's game type was the respected game type defined by the
  `AnchorStateRegistry` contract at the time of the game's creation as per a call to
  `AnchorStateRegistry.respectedGameType()`.
- MUST return `false` if the call to `FaultDisputeGame.wasRespectedGameTypeWhenCreated` reverts.

### isGameBlacklisted

Determines if a game is a Blacklisted Game.

- MUST return `true` if and only if the game's address is marked as blacklisted inside of the
  `AnchorStateRegistry` contract.

### isGameRetired

Determines if a game is a Retired Game.

- MUST return `true` if and only if the game was created before the retirement timestamp defined by
  the `AnchorStateRegistry` contract as per a call to `AnchorStateRegistry.retirementTimestamp()`.
  Check should be a strict comparison that the creation is less than the retirement timestamp.

### isGameProper

Determines if a game is a Proper Game.

- MUST return `true` if and only if `isGameRegistered(game)` is `true`, `isGameBlacklisted(game)`
  and `isGameRetired(game)` are both `false`, and `paused()` is `false`.

### isGameResolved

Determines if a game is a Resolved Game.

- MUST return `true` if and only if the game has resolved a result in favor of either the
  Challenger or the Defender as determined by the `FaultDisputeGame.status()` function.

### isGameFinalized

Determines if a game is a Finalized Game.

- MUST return `true` if and only if `isGameResolved(game)` and the game has resolved a result more
  than the airgap delay seconds ago as defined by the `disputeGameFinalityDelaySeconds` variable in
  the `AnchorStateRegistry` contract.

### isGameClaimValid

Determines if a game has a Valid Claim.

- MUST return `true` if and only if `isGameProper(game)` is `true`, `isGameRespected(game)` is
  `true`, `isGameFinalized(game)` is `true`, and the game resolved in favor of the root claim
  (i.e., in favor of the Defender).

### getAnchorRoot

Retrieves the current anchor root.

- MUST return the root hash and L2 block height of the current anchor state.

### anchors

Legacy function. Accepts a game type as a parameter but does not use it.

- MUST return the current value of `getAnchorRoot()`.

### setAnchorState

Allows any address to attempt to update the Anchor Game with a new Game as input.

- MUST revert if the provided game does not have a Valid Claim for any reason.
- MUST revert if the provided game corresponds to an L2 block height that is less than or equal
  to the current anchor state's L2 block height.
- MUST otherwise update the anchor state to match the game's result.
