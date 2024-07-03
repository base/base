# Alligator

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Subdelegations](#subdelegations)
- [Token Transfer Hook](#token-transfer-hook)
- [View functions](#view-functions)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `Alligator` contract implements subdelegations that can be used by tokens contracts that inherit
[`GovernanceToken`](gov-token.md). Subdelegations allow for advanced delegation use cases, such as partial,
time-constrained & block-based delegations, relative & fixed allowances, and custom rules. The `Alligator` contract
requires migrating the delegation state from the token contract to itself through a hook-based approach. Concretely,
token contracts call the `Alligator` contract's `afterTokenTransfer` function after a token transfer. This allows
the `Alligator` contract to consume the hooks and update its delegation and checkpoint mappings accordingly. If either
of the addresses passed as arguments to `afterTokenTransfer`, `from` and `to`, have not been migrated to the `Alligator`
contract, `Alligator` copies the address's delegation and checkpoint data from the token contract to its own state.

## Subdelegations

To support subdelegations for multiple token contracts, the `Alligator` contract MUST implement storage references to
store delegations and checkpoints for addresses. Specifically, the `Alligator` contract MUST implement the equivalent
of the following mappings from `GovernanceToken` contract: `_delegates`, `_checkpoints`, `_totalSupplyCheckpoints`.

The contract MUST also provide methods for subdelegation voting power to another address. Concretely, the `Alligator`
contract MUST implement a `subdelegate` function which takes a `to` address, and a subdelegation rule. A subdelegation
rule MUST contain the following fields:

- `maxRedelegations`: The maximum number of times the delegated votes can be redelegated.
- `blocksBeforeVoteCloses`: The number of blocks before the vote closes that the delegation is valid.
- `notValidBefore`: The timestamp after which the delegation is valid.
- `notValidAfter`: The timestamp before which the delegation is valid.
- `allowanceType` The type of allowance. If `Absolute`, the amount of votes delegated is fixed. If `Relative`, the
  amount of votes delegated is relative to the total amount of votes the delegator has.
- `allowance`: The amount of votes delegated, denominated in the token contract's decimals.

The `Alligator` contract MUST provide a method to subdelegate in batches, called `subdelegateBatched`. This function
should take an array of `to` addresses and an array of subdelegation rules, and call the `subdelegate` function for each
pair of `to` address and subdelegation rule. `Alligator` MUST also provide a method to subdelegate votes using a
signature, called `subdelegateBySig`. This function should take the same arguments as `subdelegate`, plus a signature
of the previous parameters.

## Token Transfer Hook

The `Alligator` contract MUST provide a method, referred to as `afterTokenTransfer`, that can be called by the token
contract's `_afterTokenTransfer` function. This method should update its delegation and checkpoint mappings for the
address of the token contract. Specifically, if the `to` or `from` addresses of the transfer have not been migrated,
the `Alligator` MUST migrate it by copying the delegation and checkpoint data from the token contract to its own state.

## View functions

`Alligator` MUST provide methods to get the delegation state for a token contract address. Specifically, the `Alligator`
contract MUST provide functions to get subdelegations, checkpoints, and current & past voting power of a user. The output
for these functions is conditional on whether the user address has been migrated or not. Concretely, the contract MUST
read its own state if the address has been migrated, and the token contract's state otherwise.
