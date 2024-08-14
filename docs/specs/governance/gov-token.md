# Governance Token

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
  - [Token Minting](#token-minting)
  - [Token Burning](#token-burning)
  - [Voting Power](#voting-power)
    - [Public Query Functions](#public-query-functions)
  - [Delegation](#delegation)
    - [Basic Delegation](#basic-delegation)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

| Constants      | Value                                        |
|----------------|----------------------------------------------|
| Address        | `0x4200000000000000000000000000000000000042` |
| Token name     | `Optimism`                                   |
| Token symbol   | `OP`                                         |
| Token decimals | `18`                                         |

`GovernanceToken` is an [ERC20](https://eips.ethereum.org/EIPS/eip-20) token contract that inherits from `ERC20Burnable`,
`ERC20Votes`, and `Ownable`. It allows token holders to delegate their voting power to other addresses, enabling a representative
voting system. The `GovernanceToken` currently only supports basic delegation whereby a user can delegate its entire token
balance to a specific address for voting and votes cannot be re-delegated.

### Token Minting

`GovernanceToken` MUST have a `mint(address,uint256)` function with external visibility that allows the `owner()` address
to mint an arbitrary number of new tokens to a specific address. This function MUST only be called by the contract
owner, the `MintManager`, as enforced by the `onlyOwner` modifier inherited from the `Ownable` contract.

### Token Burning

The contract MUST allow token holders to burn their own tokens using the inherited `burn(uint256)` or
`burnFrom(address,uint256)` functions inherited from `ERC20Burnable`. Burn functions MUST NOT allow a user's balance to
underflow and attempts to reduce balance below zero MUST revert.

### Voting Power

Each token corresponds to one unit of voting power. Token holders who wish to utilize their tokens for voting MUST delegate
their voting power to an address (can be their own address). An active delegation MUST NOT prevent a user from transferring
tokens. The `GovernanceToken` contract MUST offer public accessor functions for querying voting power, as outlined below.

#### Public Query Functions

- `checkpoints(address,uint32) returns (Checkpoint)`
  - MUST retrieve the n-th Checkpoint for a given address.
- `numCheckpoints(address) returns (uint32)`
  - MUST retrieve the total number of Checkpoint objects for a given address.
- `delegates(address) returns (address)`
  - MUST retrieve the address that an account is delegating to.
- `getVotes() returns (uint256)`
  - MUST retrieve the current voting power of an address.
- `getPastVotes(address,uint256) returns (uint256)`
  - MUST retrieve the voting power of an address at specific block number in the past.
- `getPastTotalSupply(uint256) returns (uint256)`
  - MUST return the total token supply at a specific block number in the past.

### Delegation

#### Basic Delegation

Vote power can be delegated either by calling the `delegate(address)` function directly (to delegate as the `msg.sender`)
or by providing a signature to be used with function `delegateBySig(address,uint256,uint256,uint8,bytes32,bytes32)`,
as inherited from `ERC20Votes`. Delegation through these functions is considered "basic delegation" as these functions will
delegate the entirety of the user's available balance to the target address. Delegations cannot be re-delegated to another
address.
