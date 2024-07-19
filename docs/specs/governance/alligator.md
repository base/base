# Alligator

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Interface](#interface)
  - [Core Functions](#core-functions)
    - [`subdelegate`](#subdelegate)
    - [`subdelegateBatched`](#subdelegatebatched)
    - [`afterTokenTransfer`](#aftertokentransfer)
  - [Getters](#getters)
    - [`getSubdelegations`](#getsubdelegations)
    - [`getCheckpoints`](#getcheckpoints)
    - [`getVotingPower`](#getvotingpower)
- [Storage](#storage)
- [Types](#types)
  - [`SubdelegationRule`](#subdelegationrule)
  - [`AllowanceType`](#allowancetype)
  - [`Checkpoint`](#checkpoint)
- [Backwards Compatibility](#backwards-compatibility)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

| Constant | Value                                        |
|----------|----------------------------------------------|
| Address  | `0x4200000000000000000000000000000000000043` |

The `Alligator` contract implements subdelegations that can be used by token contracts inheriting from
[`GovernanceToken`](gov-token.md). Subdelegations enable advanced delegation use cases, such as partial, time-constrained
& block-based delegations, and relative & fixed allowances.

The `Alligator` contract migrates the delegation state from a token contract to itself through a hook-based approach.
Specifically, the token contract calls the `Alligator` contract's `afterTokenTransfer` function after a token
transfer. This enables the `Alligator` contract to consume the hook and update its delegation and checkpoint mappings
accordingly. If either address involved in the transfer (`_from_` or `_to`) has not been migrated to the `Alligator` contract,
the contract copies the address' delegation and checkpoint data from the token contract to its own state.

## Interface

### Core Functions

#### `subdelegate`

Allows subdelegation of token voting power to another address with specified subdelegation rules. This function
is inteded to be called by users.

```solidity
subdelegate(address _token, address _delegatee, SubdelegationRule _rule)
```

#### `subdelegate`

Allows subdelegation of token voting power to another address with specified subdelegation rules. This function
is intended to be called by the token contract.

```solidity
subdelegate(address _account, address _delegatee, SubdelegationRule _rule)
```

#### `subdelegateBatched`

Allows batch subdelegation of token voting power to multiple addresses with specified subdelegation rules. This
function is intended to be called by users.

```solidity
subdelegateBatched(address _token, address[] _delegatees, SubdelegationRule[] _rules)
```

Calls the `subdelegate` function for each pair of `_delegatees` address and subdelegation rule.

#### `afterTokenTransfer`

Updates delegation and checkpoint mappings for the address of a token contract after a transfer. This function
MUST be called by the `_afterTokenTransfer` function in the token contract.

```solidity
afterTokenTransfer(address _from, address _to, uint256 _amount)
```

The `Alligator` MUST check if the `_from` or `_to` addresses have been migrated by checking the `migrated` mapping
from its [storage](#storage). If either address has not been migrated, the `Alligator` MUST copy the delegation
and checkpoint data from the token contract to its own state. After copying the data, the `Alligator` MUST update
the `migrated` mapping to reflect that the address has been migrated.

### Getters

The output for these functions is conditional on whether the user address has been migrated or not. Concretely, the
`Alligator` MUST use its own state if the address has been migrated, or else it MUST use the state of the token contract.

#### `getSubdelegations`

Retrieves the subdelegations for a given token contract and user address.

```solidity
getSubdelegations(address _token, address _user) returns (SubdelegationRule[] memory)
```

#### `getCheckpoints`

Retrieves the checkpoints for a given token contract and user address.

```solidity
getCheckpoints(address _token, address _user) returns (Checkpoint[] memory)
```

#### `getVotingPower`

Retrieves the current and past voting power for a given token contract and user address.

```solidity
getVotingPower(address _token, address _user, uint256 _blockNumber) returns (uint256)
```

## Storage

The `Alligator` contract MUST be able to store delegations and checkpoints for multiple token contracts.
These storage variables MUST be defined in the same way as in the token contract:

```solidity
  // Mapping to keep track of the subdelegations for a token contract address and a user address
  mapping(address token => mapping(address from => mapping(address to => SubdelegationRules subdelegationRules))) public subdelegations;

  // Checkpoints of votes for a toke contract address and a user address
  mapping(address token => mapping(address user => Checkpoint[] checkpoint)) internal _checkpoints;

  // Total supply checkpoints for a token contract address
  mapping(address token => Checkpoint[] checkpoint) internal _totalSupplyCheckpoints;

  // Mapping to keep track of migrated addresses from token contracts.
  mapping(address token => mapping(address user => bool migrated)) public migrated;
```

## Types

The `Alligator` contract MUST define the following types:

### `SubdelegationRule`

Subdelegation rules define the parameters and constraints for delegated voting power, encapsulated in the following struct:

```solidity
struct SubdelegationRule {
    uint256 maxRedelegations;
    uint256 blocksBeforeVoteCloses;
    uint256 notValidBefore;
    uint256 notValidAfter;
    AllowanceType allowanceType;
    uint256 allowance;
}
```

| Name                     | Type            | Description                                                             |
|--------------------------|-----------------|-------------------------------------------------------------------------|
| `maxRedelegations`       | `uint256`       | Maximum number of times the delegated votes can be redelegated.         |
| `blocksBeforeVoteCloses` | `uint256`       | Number of blocks before the vote closes that the delegation is valid.   |
| `notValidBefore`         | `uint256`       | Timestamp after which the delegation is valid.                          |
| `notValidAfter`          | `uint256`       | Timestamp before which the delegation is valid.                         |
| `allowanceType`          | `AllowanceType` | Type of allowance (e.g., absolute or relative).                         |
| `allowance`              | `uint256`       | Amount of votes delegated, denominated in the token's decimals.         |

### `AllowanceType`

Subdelegations can have different types of allowances, represented with:

```solidity
enum AllowanceType {
  Absolute,
  Relative
}
```

| Name                     | Number    | Description                                                                                 |
|--------------------------|-----------|---------------------------------------------------------------------------------------------|
| `Absolute`               | `0`       | The amount of votes delegated is fixed.                                                     |
| `Relative`               | `1`       | The amount of votes delegated is relative to the total amount of votes the delegator has.   |

### `Checkpoint`

Checkpoints are used to store the voting power of a user at a specific block number. A checkpoint MUST be an instance of the `ERC20Votes` `Checkpoint` struct:

```solidity
struct Checkpoint {
  uint32 fromBlock;
  uint224 votes;
}
```

| Name              | Type         | Description                                     |
|-------------------|--------------|-------------------------------------------------|
| `fromBlock`       | `uint32`     | Block number the checkpoint was created.        |
| `votes`           | `uint224`    | Amount of votes at the checkpoint.              |

## Backwards Compatibility

The `Alligator` contract ensures backwards compatibility by allowing the migration of delegation state from the
token contract.
