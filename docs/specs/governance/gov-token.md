# Governance Token

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
  - [Checkpoints](#checkpoints)
  - [Token Minting](#token-minting)
  - [Token Burning](#token-burning)
  - [Voting Power](#voting-power)
    - [Queries](#queries)
  - [Delegation](#delegation)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

| Constants    | Value                                        |
|--------------|----------------------------------------------|
| Address      | `0x4200000000000000000000000000000000000042` |
| Token name   | `Optimism`                                   |
| Token symbol | `OP`                                         |

`GovernanceToken` is an [ERC20](https://eips.ethereum.org/EIPS/eip-20) token contract that inherits from `ERC20Burnable`,
`ERC20Votes`, and `Ownable`. It allows token holders to delegate their voting power to other addresses, enabling a representative
voting system.

The specific implementation details of the voting and delegation logic are inherited from the `ERC20Votes` contract. The
`GovernanceToken` contract focuses on integrating this functionality with `ERC20Burnable` and adding the minting
capability. EIP-712 permit functionality is also inherited from `ERC20Permit`, which is imported by `ERC20Votes`.

### Checkpoints

`ERC20Votes` uses checkpoints to track voting power history. A checkpoint is a "snapshot" of the voting power of an
address at a specific block number.

```solidity
struct Checkpoint {
    uint32 fromBlock;
    uint224 votes;
}
```

Checkpoints are organized by a mapping of an address to an append-only list of checkpoints, one for each user. When a
user is involved in a token transfer, mint, burn, or vote delegation, their checkpoint list is incremented with a new
checkpoint reflecting the user's updated voting power.

```solidity
mapping(address => Checkpoint[]) private _checkpoints;
```

For any checkpoint list, the `fromBlock` value of each checkpoint is strictly increasing as a factor of the checkpoint's
index in the list. Additionally, if a checkpoint is created when there's already a checkpoint at the same block number,
the new checkpoint replaces the old one.

### Token Minting

`GovernanceToken` MUST have a `mint(address,uint256)` function with external visibility that allows the contract owner
to mint an arbitrary number of new tokens to an specific address. This function MUST only be called by the contract
owner, the `MintManager`, as enforced by the `onlyOwner` modifier inherited from the `Ownable` contract. When tokens
are minted, the voting power of the recipient address MUST be updated accordingly. The total token supply is capped to
`2^208^ - 1` to prevent overflow risks in the voting system. If the total supply exceeds this limit,
`_mint(address,uint256)`, as inherited from `ERC20Votes`, MUST revert.

### Token Burning

The contract MUST allow token holders to burn their own tokens using the inherited `burn(uint256)` or
`burnFrom(address,uint256)` functions inherited from `ERC20Burnable`. When tokens are burned, the total supply and the
holder's voting power MUST be reduced accordingly.

### Voting Power

Each token corresponds to one unit of voting power.
By default, token balance does not account for voting power. To have their voting power counted, token holders MUST delegate
their voting power to an address (can be their own address).
The contract MUST offer public accessors for quering voting power, as outlined below.

#### Queries

- The `getVotes()(uint256)` function MUST return the current voting power of an address.
- The `getPastVotes(address, uint256)(uint256)` function MUST allow querying the voting power of an address at a specific
  block number in the past.
- The `getPastTotalSupply(uint256)(uint256)` function MUST return the total voting power at a specific block number in
  the past.

### Delegation

Vote power can be delegated either by calling the `delegate(address)` function directly (to delegate as the `msg.sender`)
or by providing a signature to be used with function `delegateBySig(address, uint256, uint256, uint8, bytes32, bytes32)`,
as inherited from `ERC20Votes`.

The delegation is recorded in a checkpoint. When a token holder delegates their voting power, the delegated address receives
the voting power corresponding to the token holder's balance. Under token transfers, the voting power associated with the
tokens is moved from the delegate of the sender to that of the recipient.
