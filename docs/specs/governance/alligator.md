# Alligator

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Interface](#interface)
  - [Functions](#functions)
    - [`subdelegate`](#subdelegate)
    - [`subdelegateBatched`](#subdelegatebatched)
    - [`subdelegateBySig`](#subdelegatebysig)
    - [`afterTokenTransfer`](#aftertokentransfer)
  - [View Functions](#view-functions)
    - [`getSubdelegations`](#getsubdelegations)
    - [`getCheckpoints`](#getcheckpoints)
    - [`getVotingPower`](#getvotingpower)
- [Implementation](#implementation)
- [Backwards Compatibility](#backwards-compatibility)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `Alligator` contract implements subdelegations that can be used by token contracts inheriting
[`GovernanceToken`](gov-token.md). Subdelegations enable advanced delegation scenarios, such as partial,
time-constrained & block-based delegations, relative & fixed allowances, and custom rules.

The `Alligator` contract migrates the delegation state from the token contract to itself through a hook-based approach.
Specifically, the governance token contract calls the `Alligator` contract's `afterTokenTransfer` function after a token
transfer. This enables the `Alligator` contract to consume the hooks and update its delegation and checkpoint mappings
accordingly. If either address involved in the transfer (`from` or `to`) has not been migrated to the `Alligator` contract,
the contract copies the address's delegation and checkpoint data from the token contract to its own state.

## Interface

### Functions

#### `subdelegate`

Allows subdelegation of voting power to another address with specified subdelegation rules.

```solidity
subdelegate(address _to, SubdelegationRule _rule)
```

A subdelegation rule is an instance of the following struct:

```solidity
struct SubdelegationRule {
    uint256 maxRedelegations; // Maximum number of times the delegated votes can be redelegated.
    uint256 blocksBeforeVoteCloses; // Number of blocks before the vote closes that the delegation is valid.
    uint256 notValidBefore; // Timestamp after which the delegation is valid.
    uint256 notValidAfter; // Timestamp before which the delegation is valid.
    SubdelegationAllowanceType allowanceType; // Type of allowance (e.g., absolute or relative).
    uint256 allowance // Amount of votes delegated, denominated in the token contract's decimals.
}
```

#### `subdelegateBatched`

Allows batch subdelegation of voting power to multiple addresses with specified subdelegation rules.

```solidity
subdelegateBatched(address[] _to, SubdelegationRule[] _rules)
```

Calls the `subdelegate` function for each pair of `_to` address and subdelegation rule.

#### `subdelegateBySig`

Allows subdelegation of voting power using a signature.

```solidity
subdelegateBySig(address _to, SubdelegationRule _rule, bytes _signature)
```

Takes the same arguments as `subdelegate`, plus a signature of the previous parameters.

#### `afterTokenTransfer`

Updates delegation and checkpoint mappings for the address of the token contract after a token transfer. This function
is called by the `_afterTokenTransfer` function in the `GovernanceToken` contract.

```solidity
afterTokenTransfer(address _from, address _to)
```

If the `to` or `from` addresses have not been migrated, `Alligator` migrates by copying the delegation and checkpoint
data from the token contract to its own state.

### View Functions

The output for these functions is conditional on whether the user address has been migrated or not. Concretely, the
contract MUST use its own state if the address has been migrated, or else it MUST use the state of the governance token.

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

## Implementation

The `Alligator` contract stores delegations and checkpoints for multiple token contracts. It implements equivalent
mappings from the `GovernanceToken` contract: `_delegates`, `_checkpoints`, and `_totalSupplyCheckpoints`.

## Backwards Compatibility

The `Alligator` contract ensures backwards compatibility by allowing the migration of delegation state from the
governance contract. Fresh chains that already store delegation state in the `Alligator` contract do not require this
feature.
