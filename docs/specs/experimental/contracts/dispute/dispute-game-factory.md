# DisputeGameFactory

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Game UUID](#game-uuid)
  - [Game Implementation](#game-implementation)
  - [Initialization Bond](#initialization-bond)
  - [Game Clone](#game-clone)
- [Assumptions](#assumptions)
  - [aDGF-001: Owner operates within governance constraints](#adgf-001-owner-operates-within-governance-constraints)
    - [Mitigations](#mitigations)
  - [aDGF-002: Implementation contracts are valid IDisputeGame implementations](#adgf-002-implementation-contracts-are-valid-idisputegame-implementations)
    - [Mitigations](#mitigations-1)
- [Invariants](#invariants)
  - [iDGF-001: Dispute game factory properly creates dispute games](#idgf-001-dispute-game-factory-properly-creates-dispute-games)
    - [Impact](#impact)
  - [iDGF-002: Dispute game factory allows anyone to create dispute games](#idgf-002-dispute-game-factory-allows-anyone-to-create-dispute-games)
    - [Impact](#impact-1)
  - [iDGF-003: Dispute game factory games are unique](#idgf-003-dispute-game-factory-games-are-unique)
    - [Impact](#impact-2)
  - [iDGF-004: Dispute game factory can create games for any game type](#idgf-004-dispute-game-factory-can-create-games-for-any-game-type)
    - [Impact](#impact-3)
- [Function Specification](#function-specification)
  - [constructor](#constructor)
  - [initialize](#initialize)
  - [gameCount](#gamecount)
  - [games](#games)
  - [gameAtIndex](#gameatindex)
  - [create](#create)
  - [getGameUUID](#getgameuuid)
  - [findLatestGames](#findlatestgames)
  - [setImplementation (without args)](#setimplementation-without-args)
  - [setImplementation (with args)](#setimplementation-with-args)
  - [setInitBond](#setinitbond)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The DisputeGameFactory serves as the central registry and factory for creating dispute game contracts in the OP Stack
fault proof system. It manages game type implementations, tracks all created games, and enforces bond requirements for
game creation.

## Definitions

### Game UUID

A unique identifier for a dispute game computed as `keccak256(gameType || rootClaim || extraData)`. This identifier
ensures that each unique combination of game parameters can only result in one game instance.

### Game Implementation

A template contract that implements the IDisputeGame interface. The factory clones this implementation using the
Clone With Immutable Args (CWIA) pattern to create new game instances efficiently.

### Initialization Bond

The amount of ETH (in wei) required to create a dispute game of a specific game type. This bond is forwarded to the
created game's initialize function and serves as the initial stake in the dispute.

### Game Clone

A minimal proxy contract created using the CWIA pattern that delegates all calls to a [Game Implementation](#game-implementation)
while storing immutable game-specific parameters (creator address, root claim, parent hash, extra data) in its bytecode.

## Assumptions

### aDGF-001: Owner operates within governance constraints

The owner of the DisputeGameFactory is trusted to set game implementations and initialization bonds according to
governance processes. Malicious or incorrect configuration by the owner could compromise the security of the dispute
system.

#### Mitigations

- Owner is controlled by governance multisig
- Configuration changes are subject to governance review
- Monitoring of owner actions

### aDGF-002: Implementation contracts are valid IDisputeGame implementations

Implementation contracts registered via setImplementation are assumed to correctly implement the IDisputeGame interface
and initialize properly when cloned. Invalid implementations could cause game creation to fail or games to behave
incorrectly.

#### Mitigations

- Implementation contracts undergo security audits
- Testing of implementations before registration
- Gradual rollout of new game types

## Invariants

### iDGF-001: Dispute game factory properly creates dispute games

The factory successfully creates functional dispute game clones when provided with valid parameters. Created games are
properly initialized with the correct immutable arguments (creator, root claim, parent hash, extra data) and can be
queried through the factory's lookup functions.

#### Impact

**Severity: Critical**

If this invariant is violated, the factory could create non-functional or incorrectly configured games, breaking the
entire dispute resolution system and potentially allowing invalid withdrawals.

### iDGF-002: Dispute game factory allows anyone to create dispute games

Any address can create a dispute game by calling the create function with valid parameters and the required
initialization bond. There are no access restrictions on game creation beyond the bond requirement.

#### Impact

**Severity: High**

If this invariant is violated, the permissionless nature of the dispute system would be compromised, potentially
preventing legitimate challengers from creating games to dispute invalid claims.

### iDGF-003: Dispute game factory games are unique

For any given combination of game type, root claim, and extra data, at most one dispute game can be created. Once a
game exists for a specific [Game UUID](#game-uuid), attempting to create another game with the same parameters will
revert.

#### Impact

**Severity: High**

If this invariant is violated, multiple games could exist for the same dispute parameters, leading to ambiguity about
which game result should be considered authoritative. This could enable attackers to create duplicate games and
potentially exploit race conditions in systems that consume game results.

### iDGF-004: Dispute game factory can create games for any game type

The factory can create games for any game type that has a registered implementation, regardless of the specific game
type value. The factory does not restrict which game types can be used, only requiring that an implementation exists.

#### Impact

**Severity: Medium**

If this invariant is violated, the factory could arbitrarily restrict certain game types, limiting the flexibility of
the dispute system and potentially preventing the deployment of new game implementations.

## Function Specification

### constructor

Initializes the contract in a disabled state to prevent implementation contract initialization.

**Behavior:**

- MUST disable initializers to prevent the implementation contract from being initialized
- MUST set the initial reinitializer version to 1

### initialize

Initializes the proxy contract with an owner address.

**Parameters:**

- `_owner`: The address that will own the contract and have permission to configure game types

**Behavior:**

- MUST revert if not called by ProxyAdmin or ProxyAdmin owner
- MUST initialize the Ownable functionality
- MUST transfer ownership to `_owner`
- MUST only be callable once per reinitializer version

### gameCount

Returns the total number of dispute games created by this factory.

**Behavior:**

- MUST return the total number of games created
- MUST be a view function with no state changes

### games

Queries for a dispute game by its parameters.

**Parameters:**

- `_gameType`: The type of the dispute game
- `_rootClaim`: The root claim of the dispute game
- `_extraData`: Any extra data provided to the dispute game

**Behavior:**

- MUST compute the [Game UUID](#game-uuid) from the provided parameters
- MUST return the game proxy address and creation timestamp if a game exists for the given parameters
- MUST return `address(0)` and timestamp 0 if no game exists for the given parameters
- MUST be a view function with no state changes

### gameAtIndex

Returns the dispute game at a specific index in the creation order.

**Parameters:**

- `_index`: The index of the dispute game in the creation list

**Behavior:**

- MUST revert if `_index` is greater than or equal to the total game count
- MUST return the game type, creation timestamp, and proxy address for the game at the specified index
- MUST be a view function with no state changes

### create

Creates a new dispute game proxy contract.

**Parameters:**

- `_gameType`: The type of the dispute game to create
- `_rootClaim`: The root claim of the dispute game
- `_extraData`: Any extra data to provide to the created dispute game

**Behavior:**

- MUST revert if no implementation is registered for `_gameType`
- MUST revert if `msg.value` does not exactly equal the initialization bond for `_gameType`
- MUST revert if a game with the same [Game UUID](#game-uuid) already exists
- MUST clone the implementation contract using CWIA with immutable arguments: creator address, root claim, parent
  block hash, and extra data
- MUST include game type and implementation args in the clone if implementation args are configured for the game type
- MUST call initialize on the created clone, forwarding the sent ETH
- MUST make the game queryable by both its parameters and its creation index
- MUST increment the total game count
- MUST emit DisputeGameCreated event with the proxy address, game type, and root claim
- MUST return the address of the created game proxy

### getGameUUID

Computes the unique identifier for a set of dispute game parameters.

**Parameters:**

- `_gameType`: The type of the dispute game
- `_rootClaim`: The root claim of the dispute game
- `_extraData`: Any extra data for the dispute game

**Behavior:**

- MUST return `keccak256(abi.encode(_gameType, _rootClaim, _extraData))`
- MUST be a pure function with no state access

### findLatestGames

Searches for the most recent games of a specific type.

**Parameters:**

- `_gameType`: The type of game to search for
- `_start`: The index to start the reverse search from
- `_n`: The maximum number of games to return

**Behavior:**

- MUST return an empty array if `_start` is greater than or equal to the total game count
- MUST return an empty array if `_n` is zero
- MUST perform a reverse linear search from `_start` towards index 0
- MUST include only games matching `_gameType`
- MUST return at most `_n` games
- MUST return games in reverse chronological order (newest first)
- MUST include the index, metadata (GameId), timestamp, root claim, and extra data for each game
- MUST query the game proxy for root claim and extra data
- MUST be a view function with no state changes

### setImplementation (without args)

Sets the implementation contract for a specific game type without implementation args.

**Parameters:**

- `_gameType`: The type of the dispute game
- `_impl`: The implementation contract address

**Behavior:**

- MUST revert if not called by the owner
- MUST store `_impl` as the implementation for `_gameType`
- MUST emit ImplementationSet event with the implementation address and game type

### setImplementation (with args)

Sets the implementation contract and configuration args for a specific game type.

**Parameters:**

- `_gameType`: The type of the dispute game
- `_impl`: The implementation contract address
- `_args`: The configuration arguments to pass during clone creation

**Behavior:**

- MUST revert if not called by the owner
- MUST store `_impl` as the implementation for `_gameType`
- MUST store `_args` as the implementation args for `_gameType`
- MUST emit ImplementationSet event with the implementation address and game type
- MUST emit ImplementationArgsSet event with the game type and args

### setInitBond

Sets the initialization bond for a specific game type.

**Parameters:**

- `_gameType`: The type of the dispute game
- `_initBond`: The bond amount in wei

**Behavior:**

- MUST revert if not called by the owner
- MUST store `_initBond` as the initialization bond for `_gameType`
- MUST emit InitBondUpdated event with the game type and new bond amount
