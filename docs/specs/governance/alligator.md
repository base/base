# Alligator

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Interface](#interface)
  - [Core Functions](#core-functions)
    - [`subdelegate`](#subdelegate)
    - [`subdelegateBatched`](#subdelegatebatched)
    - [`subdelegateBySig`](#subdelegatebysig)
    - [`afterTokenTransfer`](#aftertokentransfer)
  - [Getters](#getters)
    - [`getSubdelegations`](#getsubdelegations)
    - [`getCheckpoints`](#getcheckpoints)
    - [`getVotingPower`](#getvotingpower)
- [Storage](#storage)
- [Backwards Compatibility](#backwards-compatibility)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `Alligator` contract implements subdelegations that can be used by token contracts inheriting from
[`GovernanceToken`](gov-token.md). Subdelegations enable advanced delegation use cases, such as partial, time-constrained
& block-based delegations, and relative & fixed allowances.

The `Alligator` contract migrates the delegation state from a token contract to itself through a hook-based approach.
Specifically, the token contract calls the `Alligator` contract's `afterTokenTransfer` function after a token
transfer. This enables the `Alligator` contract to consume the hook and update its delegation and checkpoint mappings
accordingly. If either address involved in the transfer (`from` or `to`) has not been migrated to the `Alligator` contract,
the contract copies the address' delegation and checkpoint data from the token contract to its own state.

## Interface

### Core Functions

#### `subdelegate`

Allows subdelegation of voting power to another address with specified subdelegation rules.

```solidity
subdelegate(address to, SubdelegationRule rule)
```

A subdelegation rule is an instance of the following struct:

```solidity
struct SubdelegationRule {
    uint256 maxRedelegations; // Maximum number of times the delegated votes can be redelegated.
    uint256 blocksBeforeVoteCloses; // Number of blocks before the vote closes that the delegation is valid.
    uint256 notValidBefore; // Timestamp after which the delegation is valid.
    uint256 notValidAfter; // Timestamp before which the delegation is valid.
    SubdelegationAllowanceType allowanceType; // Type of allowance (e.g., absolute or relative).
    uint256 allowance // Amount of votes delegated, denominated in the token's decimals.
}
```

#### `subdelegateBatched`

Allows batch subdelegation of voting power to multiple addresses with specified subdelegation rules.

```solidity
subdelegateBatched(address[] to, SubdelegationRule[] rules)
```

Calls the `subdelegate` function for each pair of `to` address and subdelegation rule.

#### `subdelegateBySig`

Allows subdelegation of voting power using a signature.

```solidity
subdelegateBySig(address to, SubdelegationRule rule, bytes signature)
```

Takes the same arguments as `subdelegate`, plus a signature of the previous parameters.

#### `afterTokenTransfer`

Updates delegation and checkpoint mappings for the address of a token contract after a transfer. This function
MUST be called by the `_afterTokenTransfer` function in the token contract.

```solidity
afterTokenTransfer(address from, address to, uint256 amount)
```

If the `to` or `from` addresses have not been migrated, `Alligator` migrates by copying the delegation and checkpoint
data from the token contract to its own state.

### Getters

The output for these functions is conditional on whether the user address has been migrated or not. Concretely, the
`Alligator` MUST use its own state if the address has been migrated, or else it MUST use the state of the token contract.

#### `getSubdelegations`

Retrieves the subdelegations for a given token contract address.

```solidity
getSubdelegations(address _user) returns (SubdelegationRule[] memory)
```

#### `getCheckpoints`

Retrieves the checkpoints for a given token contract address.

```solidity
getCheckpoints(address _user) returns (Checkpoint[] memory)
```

#### `getVotingPower`

Retrieves the current and past voting power of a given user.

```solidity
getVotingPower(address _user, uint256 _blockNumber) returns (uint256)
```

## Storage

The `Alligator` contract stores delegations and checkpoints for multiple token contracts. The `GovernanceToken` already
does this, which `Alligator` inherits from. However, `GovernanceToken` defines those variables as `private`, which means
that they cannot be accessed by its inheritors. Thus, we have to define them again in the `Alligator` contract.

```solidity
  mapping(address => address) internal _delegates; // Mapping to keep track of the delegates of each account

  mapping(address => Checkpoint[]) internal _checkpoints; // Checkpointing for votes for each account

  Checkpoint[] internal _totalSupplyCheckpoints; // Array of all checkpoints
```

## Backwards Compatibility

The `Alligator` contract ensures backwards compatibility by allowing the migration of delegation state from the
token contract. Fresh chains that already store delegation state in the `Alligator` contract do not require this
feature.
