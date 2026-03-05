# MintManager

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Mint Cap](#mint-cap)
  - [Mint Period](#mint-period)
- [Assumptions](#assumptions)
  - [aMM-001: GovernanceToken implements required functions correctly](#amm-001-governancetoken-implements-required-functions-correctly)
    - [Mitigations](#mitigations)
  - [aMM-002: Block timestamp reliability](#amm-002-block-timestamp-reliability)
    - [Mitigations](#mitigations-1)
  - [aMM-003: Owner acts within governance constraints](#amm-003-owner-acts-within-governance-constraints)
    - [Mitigations](#mitigations-2)
  - [aMM-004: Valid successor MintManager](#amm-004-valid-successor-mintmanager)
    - [Mitigations](#mitigations-3)
- [Dependencies](#dependencies)
- [Invariants](#invariants)
  - [iMM-001: Mint cap enforcement](#imm-001-mint-cap-enforcement)
    - [Impact](#impact)
  - [iMM-002: Time-based minting restriction](#imm-002-time-based-minting-restriction)
    - [Impact](#impact-1)
  - [iMM-003: Exclusive minting authority](#imm-003-exclusive-minting-authority)
    - [Impact](#impact-2)
  - [iMM-004: Ownership transfer control](#imm-004-ownership-transfer-control)
    - [Impact](#impact-3)
- [Function Specifications](#function-specifications)
  - [constructor](#constructor)
  - [mint](#mint)
  - [upgrade](#upgrade)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `MintManager` contract serves as the owner of the [GovernanceToken](./gov-token.md) and controls the token
inflation schedule. It enforces rate-limited minting with a maximum cap per minting operation and a minimum time
period between mints. The contract is upgradeable, allowing the owner to transfer control to a new `MintManager`
implementation if changes to the inflation schedule are required.

## Definitions

### Mint Cap

The maximum percentage of the total token supply that can be minted in a single minting operation. Set to 2%
(represented as 20/1000 with 4 decimal precision).

### Mint Period

The minimum time interval that must elapse between consecutive minting operations. Set to 365 days.

## Assumptions

### aMM-001: GovernanceToken implements required functions correctly

The `GovernanceToken` contract correctly implements the `mint(address,uint256)` function, `transferOwnership(address)`
function, and `totalSupply()` view function according to their expected behavior. Specifically:

- `mint()` increases the token balance of the specified account by the specified amount
- `totalSupply()` accurately returns the current total supply of tokens
- `transferOwnership()` transfers ownership to the specified address

#### Mitigations

- The `GovernanceToken` contract is part of the same protocol and is subject to the same security audits
- The interface is well-defined in `IGovernanceToken.sol`
- The implementation follows OpenZeppelin's standard patterns

### aMM-002: Block timestamp reliability

The EVM `block.timestamp` value is sufficiently reliable for enforcing the 365-day minting period. While miners can
manipulate timestamps within a small range (~15 seconds), this manipulation is negligible compared to the 365-day
period.

#### Mitigations

- The 365-day period is long enough that minor timestamp manipulation has no practical impact
- Ethereum consensus rules limit timestamp manipulation
- The time-based restriction is a rate limit, not a precise scheduling mechanism

### aMM-003: Owner acts within governance constraints

The contract owner (typically a governance multisig or DAO) will only call `mint()` and `upgrade()` functions in
accordance with governance decisions and the protocol's established rules. The owner will not abuse their authority
to mint excessive tokens or transfer ownership to malicious addresses.

#### Mitigations

- Owner is expected to be a governance-controlled address (e.g., multisig or Governor contract)
- All minting operations are subject to the on-chain mint cap and time restrictions
- Ownership transfers are transparent on-chain and subject to community oversight
- The upgrade mechanism allows for replacing a compromised or malicious owner

### aMM-004: Valid successor MintManager

When upgrading, the owner will provide a valid, non-zero address for the new `MintManager` contract. The successor
contract will be properly implemented and tested before the upgrade is executed.

#### Mitigations

- Governance processes include review and testing of new MintManager implementations
- The upgrade transaction is subject to governance approval and timelock mechanisms
- The contract enforces a basic check that the successor address is not the zero address

## Dependencies

This specification depends on:

- [GovernanceToken](./gov-token.md) - The ERC20Votes token that this contract has permission to mint

## Invariants

### iMM-001: Mint cap enforcement

No single minting operation can mint more than the [Mint Cap](#mint-cap) of the current total token supply.

**Full Description:**
Any call to `mint()` MUST enforce that the requested mint amount does not exceed the [Mint Cap](#mint-cap). This
ensures that token inflation is bounded and predictable.

#### Impact

**Severity: Medium**

If this invariant is violated, the owner could mint an unlimited number of tokens, leading to:
- Severe token dilution for existing holders
- Loss of governance voting power for existing token holders
- Destruction of the token's economic value
- Complete loss of trust in the protocol's governance system

Note: This is rated Medium because it requires assumption aMM-003 (owner acts within governance constraints) to fail.
If aMM-003 does not hold (i.e., the owner is malicious or compromised), this would be elevated to Critical severity.
The contract enforces this invariant on-chain, providing defense-in-depth against governance failures.

### iMM-002: Time-based minting restriction

Minting operations can only occur after the [Mint Period](#mint-period) has elapsed since the previous mint.

**Full Description:**
Any call to `mint()` MUST revert if the [Mint Period](#mint-period) has not elapsed since the last mint. This ensures
that minting operations are rate-limited, preventing rapid inflation even if the owner attempts multiple mints.

#### Impact

**Severity: Medium**

If this invariant is violated, the owner could:
- Mint the [Mint Cap](#mint-cap) multiple times in rapid succession
- Cause uncontrolled inflation far exceeding the intended rate
- Undermine the predictability and transparency of the token supply schedule
- Violate the expectations of token holders regarding inflation rates

Note: This is rated Medium because it requires assumption aMM-003 (owner acts within governance constraints) to fail.
If aMM-003 does not hold (i.e., the owner is malicious or compromised), this would be elevated to Critical severity.
The contract enforces this invariant on-chain, providing defense-in-depth against governance failures.

### iMM-003: Exclusive minting authority

Only the contract owner can successfully call the `mint()` function to create new governance tokens.

**Full Description:**
The `mint()` function MUST be protected by the `onlyOwner` modifier, ensuring that only the address returned by
`owner()` can execute minting operations. Any call from a non-owner address MUST revert.

#### Impact

**Severity: Critical**

If this invariant is violated:
- Unauthorized parties could mint tokens without governance approval
- The entire governance system would be compromised
- Token supply would become unpredictable and uncontrolled
- The economic security of the protocol would be destroyed

### iMM-004: Ownership transfer control

Only the current contract owner can transfer ownership of the GovernanceToken to a new MintManager.

**Full Description:**
The `upgrade()` function MUST be protected by the `onlyOwner` modifier, ensuring that only the current owner can
initiate an upgrade to a new `MintManager` implementation. This prevents unauthorized parties from taking control of
the token minting authority.

#### Impact

**Severity: Critical**

If this invariant is violated:
- Attackers could transfer ownership to a malicious contract
- The governance system would lose control over token minting
- A malicious MintManager could mint unlimited tokens or implement harmful policies
- The protocol's governance would be permanently compromised

## Function Specifications

### constructor

```solidity
constructor(address _upgrader, address _governanceToken)
```

Initializes the `MintManager` contract with the specified owner and governance token.

**Parameters:**
- `_upgrader`: The address that will become the owner of this contract
- `_governanceToken`: The address of the `GovernanceToken` contract that this manager will control

**Behavior:**
- MUST call `transferOwnership(_upgrader)` to set the contract owner
- MUST set `governanceToken` to the provided `_governanceToken` address
- MUST initialize `mintPermittedAfter` to enable immediate first mint while enforcing restrictions on subsequent mints
- MUST NOT validate that `_upgrader` or `_governanceToken` are non-zero addresses (caller responsibility)

### mint

```solidity
function mint(address _account, uint256 _amount) public onlyOwner
```

Mints new governance tokens to the specified account, subject to time and cap restrictions.

**Parameters:**
- `_account`: The address that will receive the newly minted tokens
- `_amount`: The number of tokens to mint (in wei, with 18 decimals)

**Behavior:**
- MUST revert if caller is not the contract owner (enforced by `onlyOwner` modifier)
- MUST revert if the [Mint Period](#mint-period) has not elapsed since the last mint
- MUST revert if `_amount` exceeds the [Mint Cap](#mint-cap)
- MUST set `mintPermittedAfter` to `block.timestamp + MINT_PERIOD` to enforce the next [Mint Period](#mint-period)
- MUST call `governanceToken.mint(_account, _amount)` to perform the actual minting

### upgrade

```solidity
function upgrade(address _newMintManager) public onlyOwner
```

Transfers ownership of the `GovernanceToken` to a new `MintManager` contract, effectively upgrading the minting system.

**Parameters:**
- `_newMintManager`: The address of the new `MintManager` contract that will become the owner of the `GovernanceToken`

**Behavior:**
- MUST revert if caller is not the contract owner (enforced by `onlyOwner` modifier)
- MUST revert if `_newMintManager == address(0)`
- MUST call `governanceToken.transferOwnership(_newMintManager)` to transfer ownership
