# Governance Token

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
  - [Token Minting](#token-minting)
  - [Token Burning](#token-burning)
  - [Voting Power](#voting-power)
    - [Queries](#queries)
  - [Delegation](#delegation)
  - [Checkpoints](#checkpoints)
  - [Overrides](#overrides)
  - [Supply Cap](#supply-cap)

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
capability.

### Checkpoints

`ERC20Votes` uses checkpoints to track voting power history. A checkpoint is a "snapshot" of the voting power of an address at a specific
block number.
```solidity
struct Checkpoint {
    uint32 fromBlock;
    uint224 votes;
}
```
Checkpoints correrspond to a mapping of an address to an append-only list of checkpoints, which are updated when tokens
are transferred, minted, burned, or delegated. Generically, updating checkpoints happens by means of the `_moveVotingPower` function.

```solidity
mapping(address => Checkpoint[]) private _checkpoints;
```

An account's checkpoints' `fromBlock` values are strictly increasing, which means checkpoints at the same block number get
overwritten by the new one, ensuring there's always at most one checkpoint per block number. This is enforced by the `_writeCheckpoint` function.

### Token Minting

The contract MUST have a `mint` function with external visibility that allows the contract owner to mint new tokens to an
arbitrary address. This function MUST only be called by the contract owner, the `MintManager`, as enforced by the `onlyOwner` modifier inherited
from the `Ownable` contract. When tokens are minted, the voting power of the recipient address MUST be updated accordingly.

### Token Burning

The contract MUST allow token holders to burn their own tokens using the inherited `burn` function from `ERC20Burnable`.
When tokens are burned, the total supply and the holder's voting power MUST be reduced accordingly.

### Voting Power

Each token corresponds to one unit of voting power.
By default, token balance does not account for voting power. To have their voting power counted, token holders MUST delegate
their voting power to an address (can be their own address).
The contract MUST offer public accessors for quering voting power, as outlined below.

#### Queries

- The `getVotes` function MUST return the current voting power of an address.
- The `getPastVotes` function MUST allow querying the voting power of an address at a specific block number in the past.
- The `getPastTotalSupply` function MUST return the total voting power at a specific block number in the past.

### Delegation

Vote power can be delegated either by calling the `delegate` function directly or by providing a signature to be used
with `delegateBySig`, as inherited from `ERC20Votes`. 

The delegation is recorded in a checkpoint. When a token holder delegates their voting power, the delegated address receives
the voting power corresponding to the token holder's balance. These tokens become independent of the user's balance, so their
transfers do not affect the voting power of the delegated address. The delegated address can further transfer these tokens,
which would move the voting power to the new recipient address

### Checkpoints

The contract keeps a history of each account's voting power using checkpoints.
Checkpoints are recorded whenever tokens are transferred, minted, or burned, and when delegation occurs.
Each checkpoint represents the voting power of an address at a specific block number.
The contract MUST maintain a mapping of addresses to their checkpoint history. Additionally, the contract MUST
provide functions `numCheckpoints` and `checkpoints` to allow retrieving the number of checkpoints for an account
and the details of a specific checkpoint.

### Supply Cap

The total token supply is capped to `2^208^ - 1` to prevent overflow risks in the voting system.
If the total supply exceeds this limit, the `_mint` function MUST revert with an `ERC20ExceededSafeSupply` error.
