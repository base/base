# Alligator

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Integration with `GovernanceToken`](#integration-with-governancetoken)
- [Governor](#governor)
- [Subdelegations](#subdelegations)
  - [Subdelegation Rules](#subdelegation-rules)
- [`afterTokenTransfer`](#aftertokentransfer)
- [View functions](#view-functions)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `Alligator` contract implements advanced delegation features for the `GovernanceToken` contract. Our goal 
is to shift the logic from the `GovernanceToken` contract to the `Alligator` contract. The `Alligator` contract
integrates with the `GovernanceToken` contract through a hook-based approach. Concretely, the `GovernanceToken`
contract calls the `Alligator` contract's `afterTokenTransfer` function after a token transfer. This
allows the `Alligator` contract to consume the hooks and update its delegation and checkpoint mappings accordingly.
If either of the addresses passed as arguments to `afterTokenTransfer`, `from` and `to`, have not been migrated to the
`Alligator` contract, `Alligator` copies the address's delegation and checkpoint data from the `GovernanceToken`
contract to its own state (migrating the address). The `Alligator` contract also provides a method for validating
subdelegation rules and partial delegation allowances.

This implementation is based on the [Alligator V5 contract](https://github.com/voteagora/governor/blob/main/src/alligator/AlligatorOP_V5.sol),
the most recent version of the Alligator contract. 

## Governor

The `Alligator` contract MUST provide a method for validating subdelegation rules and casting a vote on the governor.
It should also provide methods for variants such as for

- casting votes with reason

- casting votes with reason and custom parameters

- casting multiple votes with reason

- limitedCastVoteWithReasonAndParamsBatched

- casting multiple votes with reason and parameters while limiting the maximum voting power allowed

- casting votes by signature

- casting votes by signature with reason and parameters

- casting multiple votes by signature with reason and parameters while limiting the maximum voting power allowed

## Subdelegations

The `Alligator` contract MUST support subdelegations, allowing for advanced delegation use cases, such as partial,
time-constrained & block-based delegations, relative & fixed allowances, and custom rules. The
`_afterTokenTransfer` function in the `GovernanceToken` is modified to call the `afterTokenTransfer` function in
the `Alligator` contract, allowing the `Alligator` contract to consume the hooks and update its delegation and
checkpoint mappings accordingly.

The contract MUST provide a method for subdelegating voting power to another address, according to a set of
subdelegation rules. It MUST also provide methods for subdelegating to multiple target addresses and for
multiple subdelegation rules. The latter is achieved through a one-to-one relationship between the subdelegation
rules and the target addresses.

### Subdelegation Rules

The subdelegation rules are a struct that MUST contain the following fields:

- `maxRedelegations`: The maximum number of times the delegated votes can be redelegated.

- `blocksBeforeVoteCloses`: The number of blocks before the vote closes that the delegation is valid.

- `notValidBefore`: The timestamp after which the delegation is valid.

- `notValidAfter`: The timestamp before which the delegation is valid.

- `customRule`: The address of a contract that implements the `IRule` interface.

- `allowanceType`: The type of allowance. If Absolute, the amount of votes delegated is fixed. If Relative, the
amount of votes delegated is relative to the total amount of votes the delegator has.

- `allowance`: The amount of votes delegated. If `allowanceType` is Relative 100% of allowance corresponds to 1e5,
otherwise this is the exact amount of votes delegated.

## `afterTokenTransfer`

`Alligator` MUST also provide a method, referred to as `afterTokenTransfer`, that can be called by the
`GovernanceToken` contract's `_afterTokenTransfer` function. This method should update the delegation and
checkpoint mappings. Specifically, if the `to` or `from` addresses have not been migrated, the `Alligator`
migrates it by copying the delegation and checkpoint data from the`GovernanceToken` contract to its own state.

## View functions

`Alligator` MUST provide a method for validating subdelegation rules and partial delegation allowances. It MUST
also provide a method for determining whether an address has been migrated or not. This method MUST return a
boolean value.

The contract must provide functions for getting the delegates for an address, a checkpoint for it at an arbitrary
position, and the address's number of checkpoints. The output for these functions is conditional on whether the
address has been migrated or not. Concretely, the contract MUST read its own state if the address has been migrated,
and the `GovernanceToken` contract's state otherwise.
