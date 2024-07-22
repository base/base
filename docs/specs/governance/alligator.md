# Alligator

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Interface](#interface)
  - [Core Functions](#core-functions)
    - [`subdelegate`](#subdelegate)
    - [`subdelegateFromToken`](#subdelegatefromtoken)
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

The `Alligator` contract implements subdelegations for the [`GovernanceToken`](gov-token.md). Subdelegations enable
advanced delegation use cases, such as partial, time-constrained & block-based delegations, and relative & fixed allowances.

The `Alligator` contract migrates the delegation state from the [`GovernanceToken`](gov-token.md) to itself
through a hook-based approach. Specifically, the [`GovernanceToken`](gov-token.md) calls the `Alligator` contract's
`afterTokenTransfer` function after a token transfer. This enables the `Alligator` contract to consume the hook and update
its delegation and checkpoint mappings accordingly. If either address involved in the transfer (`_from_` or `_to`) has not
been migrated to the `Alligator` contract, the contract copies the address' checkpoint data from the
[`GovernanceToken`](gov-token.md) to its own state.

## Interface

### Core Functions

#### `subdelegate`

Allows subdelegation of token voting power to another address (delegatee) with specified subdelegation rules. This function
is inteded to be called by users that require advanced delegation of the [`GovernanceToken`](gov-token.md).

```solidity
subdelegate(address _delegatee, SubdelegationRule _rule)
```

#### `subdelegateFromToken`

Allows subdelegation of token voting power from an address (delegator) to another address (delegatee) with a specified
subdelegation rule. This function is intended to be called by the [`GovernanceToken`](gov-token.md) contract as part
of its `delegateBySig` function. To ensure backwards compatibility in the [`GovernanceToken`](gov-token.md), the
subdelegation rule must be 100% delegation, mimicking the behavior of the [`GovernanceToken`](gov-token.md)'s `delegate`
function.


```solidity
subdelegateFromToken(address _delegator, address _delegatee, SubdelegationRule _rule)
```

#### `subdelegateBatched`

Allows batch subdelegation of token voting power to multiple addresses with specified subdelegation rules. This
function is intended to be called by users.

```solidity
subdelegateBatched(address[] _delegatees, SubdelegationRule[] _rules)
```

Calls the `subdelegate` function for each pair of `_delegatees` address and subdelegation rule.

#### `afterTokenTransfer`

Updates the voting power of two addresses (`_from` and `_to`) after a token transfer. This function MUST
be called by the `_afterTokenTransfer` function in the [`GovernanceToken`](gov-token.md).

```solidity
afterTokenTransfer(address _from, address _to, uint256 _amount)
```

The `Alligator` MUST check if the `_from` or `_to` addresses have been migrated by checking the `migrated` mapping
from its [storage](#storage). If either address has not been migrated, the `Alligator` MUST copy the delegation
and checkpoint data from the token contract to its own state. After copying the data, the `Alligator` MUST update
the `migrated` mapping to reflect that the address has been migrated.

### Getters

For backwards compatibility, the `Alligator` MUST implement all public getter functios of the
[`GovernanceToken`](gov-token.md) related to delegation and voting power. These functions MUST be used by the
[`GovernanceToken`](gov-token.md) when an account has been been migrated to the `Alligator` contract. Otherwise,
the [`GovernanceToken`](gov-token.md) MUST use its own state.

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
