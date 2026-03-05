# GovernanceToken

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [MintManager](#mintmanager)
- [Assumptions](#assumptions)
  - [aGT-001: Owner operates within governance constraints](#agt-001-owner-operates-within-governance-constraints)
    - [Mitigations](#mitigations)
- [Invariants](#invariants)
  - [iGT-001: Voting power reflects delegated balance](#igt-001-voting-power-reflects-delegated-balance)
    - [Impact](#impact)
  - [iGT-002: Delegation does not restrict transfers](#igt-002-delegation-does-not-restrict-transfers)
    - [Impact](#impact-1)
  - [iGT-003: Unauthorized minting is impossible](#igt-003-unauthorized-minting-is-impossible)
    - [Impact](#impact-2)
  - [iGT-004: Standard ERC20Votes compliance](#igt-004-standard-erc20votes-compliance)
    - [Impact](#impact-3)
- [Function Specification](#function-specification)
  - [mint](#mint)
  - [burn](#burn)
  - [burnFrom](#burnfrom)
  - [delegate](#delegate)
  - [delegateBySig](#delegatebysig)
  - [getVotes](#getvotes)
  - [getPastVotes](#getpastvotes)
  - [getPastTotalSupply](#getpasttotalsupply)
  - [delegates](#delegates)
  - [transfer](#transfer)
  - [transferFrom](#transferfrom)
  - [approve](#approve)
  - [permit](#permit)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The GovernanceToken is an ERC20 token that serves as the governance token for the OP Stack. It enables token holders to
participate in governance through voting and delegation mechanisms while supporting controlled token inflation through
owner-managed minting.

## Definitions

### MintManager

The contract that owns the GovernanceToken and has exclusive authority to mint new tokens. The MintManager enforces
on-chain rate limits and caps on minting operations according to governance-approved token inflation schedules.

## Assumptions

### aGT-001: Owner operates within governance constraints

The contract owner ([MintManager](#mintmanager)) is trusted to mint tokens only in accordance with governance decisions
and established protocol rules.

#### Mitigations

- Owner is expected to be a governance-controlled [MintManager](#mintmanager) contract
- [MintManager](#mintmanager) enforces on-chain rate limits and caps on minting operations
- Ownership transfers are transparent and subject to community oversight

## Invariants

### iGT-001: Voting power reflects delegated balance

The voting power of any address always equals the sum of token balances that have been delegated to that address.

#### Impact

**Severity: High**

If this invariant is violated, governance voting would become unreliable. Addresses could have voting power that
doesn't correspond to actual delegated tokens, allowing manipulation of governance decisions and undermining the
integrity of the voting system.

### iGT-002: Delegation does not restrict transfers

Token holders can always transfer their tokens regardless of whether they have delegated their voting power or whether
their tokens have been delegated to them by others.

#### Impact

**Severity: High**

If this invariant is violated, tokens would become illiquid when delegated, preventing normal token transfers and
breaking the fungibility of the token. This would severely impact the token's utility and market functionality.

### iGT-003: Unauthorized minting is impossible

Only the contract owner can successfully mint new tokens through the mint function.

#### Impact

**Severity: Critical**

If this invariant is violated, unauthorized parties could mint unlimited tokens, completely destroying the token's
economic value, diluting existing holders, and compromising the entire governance system.

### iGT-004: Standard ERC20Votes compliance

The contract maintains full compatibility with ERC20, ERC20Votes, ERC20Permit, and ERC20Burnable standards without
custom modifications that break standard behavior.

#### Impact

**Severity: Critical**

If this invariant is violated, the token would lose compatibility with standard governance systems, wallets, and DeFi
protocols. Governance contracts expecting standard ERC20Votes behavior would malfunction, voting power calculations
could become incorrect, and the token would not be usable in standard governance frameworks like OpenZeppelin Governor
or Compound-style governance systems.

## Function Specification

### mint

```solidity
function mint(address _account, uint256 _amount) public onlyOwner
```

Mints new governance tokens to the specified account.

**Parameters:**
- `_account`: The address that will receive the newly minted tokens
- `_amount`: The number of tokens to mint (in wei, with 18 decimals)

**Behavior:**
- MUST revert if caller is not the contract owner
- MUST mint `_amount` tokens to `_account`
- MUST increase total supply by `_amount`
- MUST update voting power for `_account` if they have delegated to themselves or if another address has delegated to
  them
- MUST emit Transfer event from zero address to `_account`
- MUST revert if total supply would exceed 2^208 - 1

### burn

```solidity
function burn(uint256 amount) public
```

Allows token holders to burn their own tokens.

**Parameters:**
- `amount`: The number of tokens to burn

**Behavior:**
- MUST revert if caller's balance is less than `amount`
- MUST reduce caller's balance by `amount`
- MUST reduce total supply by `amount`
- MUST update voting power for the caller's delegate
- MUST emit Transfer event from caller to zero address

### burnFrom

```solidity
function burnFrom(address account, uint256 amount) public
```

Allows burning tokens from another account with proper allowance.

**Parameters:**
- `account`: The address from which tokens will be burned
- `amount`: The number of tokens to burn

**Behavior:**
- MUST revert if caller's allowance for `account` is less than `amount`
- MUST revert if `account`'s balance is less than `amount`
- MUST reduce `account`'s balance by `amount`
- MUST reduce caller's allowance by `amount`
- MUST reduce total supply by `amount`
- MUST update voting power for `account`'s delegate
- MUST emit Transfer event from `account` to zero address
- MUST emit Approval event reflecting the reduced allowance

### delegate

```solidity
function delegate(address delegatee) public
```

Delegates the caller's voting power to another address.

**Parameters:**
- `delegatee`: The address to delegate voting power to (can be the caller's own address)

**Behavior:**
- MUST update the delegation mapping to record that caller delegates to `delegatee`
- MUST transfer voting power from the caller's previous delegate to the new `delegatee`
- MUST create a checkpoint recording the voting power change
- MUST emit DelegateChanged event with the caller, previous delegate, and new delegate
- MUST emit DelegateVotesChanged events for both the previous and new delegates
- MUST NOT prevent the caller from transferring tokens after delegation

### delegateBySig

```solidity
function delegateBySig(address delegatee, uint256 nonce, uint256 expiry, uint8 v, bytes32 r, bytes32 s) public
```

Delegates voting power using an EIP-712 signature, enabling gasless delegation.

**Parameters:**
- `delegatee`: The address to delegate voting power to
- `nonce`: The nonce for replay protection
- `expiry`: The timestamp after which the signature expires
- `v`: The recovery byte of the signature
- `r`: The first 32 bytes of the signature
- `s`: The second 32 bytes of the signature

**Behavior:**
- MUST revert if the signature has expired (block.timestamp > expiry)
- MUST revert if the nonce is incorrect for the signer
- MUST recover the signer address from the signature
- MUST revert if signature is invalid
- MUST perform delegation on behalf of the recovered signer address
- MUST increment the nonce for the signer
- MUST emit DelegateChanged and DelegateVotesChanged events as in delegate function

### getVotes

```solidity
function getVotes(address account) public view returns (uint256)
```

Returns the current voting power of an address.

**Parameters:**
- `account`: The address to query

**Behavior:**
- MUST return the current voting power delegated to `account`
- MUST reflect all delegations made up to the current block
- MUST NOT include tokens held by `account` unless they have delegated to themselves

### getPastVotes

```solidity
function getPastVotes(address account, uint256 blockNumber) public view returns (uint256)
```

Returns the voting power of an address at a specific block in the past.

**Parameters:**
- `account`: The address to query
- `blockNumber`: The block number to query (must be in the past)

**Behavior:**
- MUST revert if `blockNumber` is greater than or equal to the current block number
- MUST return the voting power delegated to `account` at the specified block
- MUST use checkpoints to efficiently retrieve historical voting power

### getPastTotalSupply

```solidity
function getPastTotalSupply(uint256 blockNumber) public view returns (uint256)
```

Returns the total voting power at a specific block in the past.

**Parameters:**
- `blockNumber`: The block number to query (must be in the past)

**Behavior:**
- MUST revert if `blockNumber` is greater than or equal to the current block number
- MUST return the total supply of tokens at the specified block
- MUST use checkpoints to efficiently retrieve historical total supply

### delegates

```solidity
function delegates(address account) public view returns (address)
```

Returns the address that an account has delegated their voting power to.

**Parameters:**
- `account`: The address to query

**Behavior:**
- MUST return the address that `account` is currently delegating to
- MUST return the zero address if `account` has never delegated

### transfer

```solidity
function transfer(address to, uint256 amount) public returns (bool)
```

Transfers tokens from the caller to another address.

**Parameters:**
- `to`: The recipient address
- `amount`: The number of tokens to transfer

**Behavior:**
- MUST revert if caller's balance is less than `amount`
- MUST revert if `to` is the zero address
- MUST reduce caller's balance by `amount`
- MUST increase `to`'s balance by `amount`
- MUST update voting power for both the caller's delegate and `to`'s delegate
- MUST emit Transfer event
- MUST return true on success

### transferFrom

```solidity
function transferFrom(address from, address to, uint256 amount) public returns (bool)
```

Transfers tokens from one address to another using allowance mechanism.

**Parameters:**
- `from`: The address to transfer tokens from
- `to`: The recipient address
- `amount`: The number of tokens to transfer

**Behavior:**
- MUST revert if caller's allowance for `from` is less than `amount` (unless allowance is max uint256)
- MUST revert if `from`'s balance is less than `amount`
- MUST revert if `to` is the zero address
- MUST reduce `from`'s balance by `amount`
- MUST increase `to`'s balance by `amount`
- MUST reduce caller's allowance by `amount` (unless allowance is max uint256)
- MUST update voting power for both `from`'s delegate and `to`'s delegate
- MUST emit Transfer event
- MUST emit Approval event if allowance was reduced
- MUST return true on success

### approve

```solidity
function approve(address spender, uint256 amount) public returns (bool)
```

Approves another address to spend tokens on behalf of the caller.

**Parameters:**
- `spender`: The address authorized to spend tokens
- `amount`: The number of tokens approved for spending

**Behavior:**
- MUST set the allowance for `spender` to `amount`
- MUST emit Approval event
- MUST return true on success
- MUST allow setting allowance to zero to revoke approval

### permit

```solidity
function permit(address owner, address spender, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s) public
```

Approves spending using an EIP-2612 signature, enabling gasless approvals.

**Parameters:**
- `owner`: The address granting the approval
- `spender`: The address receiving the approval
- `value`: The amount approved for spending
- `deadline`: The timestamp after which the signature expires
- `v`: The recovery byte of the signature
- `r`: The first 32 bytes of the signature
- `s`: The second 32 bytes of the signature

**Behavior:**
- MUST revert if the signature has expired (block.timestamp > deadline)
- MUST recover the signer address from the EIP-712 signature
- MUST revert if recovered signer does not match `owner`
- MUST revert if signature is invalid
- MUST set the allowance for `spender` to `value`
- MUST increment the nonce for `owner`
- MUST emit Approval event
