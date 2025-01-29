# AnchorStateRegistry

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Dispute Game](#dispute-game)
  - [Respected Game Type](#respected-game-type)
  - [Registered Game](#registered-game)
  - [Respected Game](#respected-game)
  - [Blacklisted Game](#blacklisted-game)
  - [Retired Game](#retired-game)
  - [Proper Game](#proper-game)
- [Assumptions](#assumptions)
  - [aASR-001: Dispute Game contracts properly report important properties](#aasr-001-dispute-game-contracts-properly-report-important-properties)
    - [Mitigations](#mitigations)
  - [aASR-002: Dispute Game Factory properly reports its created games](#aasr-002-dispute-game-factory-properly-reports-its-created-games)
    - [Mitigations](#mitigations-1)
  - [aASR-003: OptimismPortal properly reports respected game type, blacklist, and retirement time](#aasr-003-optimismportal-properly-reports-respected-game-type-blacklist-and-retirement-time)
    - [Mitigations](#mitigations-2)
- [Invariants](#invariants)
  - [iASR-001: Games are represented as Proper Games accurately](#iasr-001-games-are-represented-as-proper-games-accurately)
    - [Impact](#impact)
    - [Dependencies](#dependencies)
- [Function Specification](#function-specification)
  - [isGameRegistered](#isgameregistered)
  - [isGameRespected](#isgamerespected)
  - [isGameBlacklisted](#isgameblacklisted)
  - [isGameRetired](#isgameretired)
  - [isGameProper](#isgameproper)
  - [getAnchorRoot](#getanchorroot)
  - [anchors](#anchors)
  - [tryUpdateAnchorState](#tryupdateanchorstate)

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
output root at a given L2 block height.

### Respected Game Type

The `OptimismPortal` contract defines a "respected game type" which is the Dispute Game type that
the portal allows to be used for the purpose of proving and finalizing withdrawals. This mechanism
allows the system to use multiple game types simultaneously while still ensuring that the
`OptimismPortal` contract only trusts respected games specifically.

### Registered Game

A Dispute Game is considered to be a **Registered Game** if the game contract was created by the
system's `DisputeGameFactory` contract.

### Respected Game

A Dispute Game is considered to be a **Respected Game** if the game contract's game type is
currently the `respectedGameType` defined by the `OptimismPortal` contract. Games that are not
Respected Games cannot be used as an Anchor Game. See [Respected Game Type](#respected-game-type)
for more information.

### Blacklisted Game

A Dispute Game is considered to be a **Blacklisted Game** if the game contract's address is marked
as blacklisted inside of the `OptimismPortal` contract.

### Retired Game

A Dispute Game is considered to be a **Retired Game** if the game contract was created before the
retirement timestamp (`respectedGameTypeUpdatedAt`) defined in the `OptimismPortal` contract.

### Proper Game

A Dispute Game is considered to be a **Proper Game** if all of the following are true:

- Game is a Registered Game
- Game is a Respected Game
- Game is not a Blacklisted Game
- Game is not a Retired Game

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
- Creation timestamp
- Resolution timestamp

#### Mitigations

- Existing audit on the `FaultDisputeGame` contract
- Integration testing

### aASR-002: Dispute Game Factory properly reports its created games

We assume that the `DisputeGameFactory` contract properly and faithfully reports the games it has
created.

#### Mitigations

- Existing audit on the `DisputeGameFactory` contract
- Integration testing

### aASR-003: OptimismPortal properly reports respected game type, blacklist, and retirement time

We assume that the `OptimismPortal` contract properly and faithfully reports the respected game
type, blacklist, and retirement timestamp.

#### Mitigations

- Existing audit on the `OptimismPortal` contract
- Integration testing

## Invariants

### iASR-001: Games are represented as Proper Games accurately

When asked if a game is a Proper Game, the `AnchorStateRegistry` must serve a response that is
identical to the response that would be given by a perfect oracle for this query.

#### Impact

**Severity: High**

If this invariant is broken, the anchor state could be set to an incorrect value, which would cause
future Dispute Game instances to use an incorrect starting state. This would lead games to resolve
incorrectly.

#### Dependencies

- [aASR-001](#aasr-001-dispute-game-contracts-properly-report-important-properties)
- [aASR-002](#aasr-002-dispute-game-factory-properly-reports-its-created-games)
- [aASR-003](#aasr-003-optimismportal-properly-reports-respected-game-type-blacklist-and-retirement-time)

## Function Specification

### isGameRegistered

Determines if a game is a Registered Game.

- MUST return `true` if and only if the game was created by the system's `DisputeGameFactory` contract.

### isGameRespected

Determines if a game is a Respected Game.

- MUST return `true` if and only if the game's game type is the respected game type defined by the
  `OptimismPortal` contract as per a call to `OptimismPortal.respectedGameType()`.

### isGameBlacklisted

Determines if a game is a Blacklisted Game.

- MUST return `true` if and only if the game's address is marked as blacklisted inside of the
  `OptimismPortal` contract as per a call to `OptimismPortal.disputeGameBlacklist(game)`.

### isGameRetired

Determines if a game is a Retired Game.

- MUST return `true` if and only if the game was created before the retirement timestamp defined by
  the `OptimismPortal` contract as per a call to `OptimismPortal.respectedGameTypeUpdatedAt()`.
  Check should be a strict comparison that the creation is less than the retirement timestamp.

### isGameProper

Determines if a game is a Proper Game.

- MUST return `true` if and only if `isGameRegistered(game)` and `isGameRespected(game)` are both
  `true` and `isGameBlacklisted(game)` and `isGameRetired(game)` are both `false`.

### getAnchorRoot

Retrieves the current anchor root.

- MUST return the root hash and L2 block height of the current anchor state.

### anchors

Legacy function. Accepts a game type as a parameter but does not use it.

- MUST return the current value of `getAnchorRoot()`.

### tryUpdateAnchorState

Allows a `DisputeGame` contract to attempt to set its own result as the anchor state. The
`AnchorStateRegistry` contract previously stored the anchor state separately for each game type.
The updated version of this function allows any game type to update the anchor state as long as it
is the currently respected game type.

- MUST not revert if the caller is a Registered Game and sufficient gas is provided.
- MUST not update the anchor state if the game is not a Proper Game for any reason.
- MUST not update the anchor state if the game is not resolved (`IN_PROGRESS`).
- MUST not update the anchor state if the game resolved in favor of the Challenger.
- MUST not update the anchor state if the game corresponds to an L2 block height that is less than
  or equal to the current anchor state's L2 block height.
- MUST otherwise update the anchor state to match the game's result.
