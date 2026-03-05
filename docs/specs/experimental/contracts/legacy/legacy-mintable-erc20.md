# LegacyMintableERC20

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
- [Assumptions](#assumptions)
- [Invariants](#invariants)
- [Function Specification](#function-specification)
  - [constructor](#constructor)
  - [supportsInterface](#supportsinterface)
  - [mint](#mint)
  - [burn](#burn)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

Deprecated legacy implementation of a mintable ERC20 token for cross-domain token bridging. This contract represents
an L1 token on L2 and allows only the designated L2 bridge to mint and burn tokens. This contract should no longer be
used for new deployments.

## Definitions

N/A

## Assumptions

N/A

## Invariants

N/A

## Function Specification

### constructor

Initializes the token with bridge and L1 token addresses.

**Parameters:**

- `_l2Bridge`: Address of the L2 standard bridge authorized to mint and burn tokens
- `_l1Token`: Address of the corresponding token on L1
- `_name`: ERC20 token name
- `_symbol`: ERC20 token symbol

**Behavior:**

- MUST set `l2Bridge` to `_l2Bridge`
- MUST set `l1Token` to `_l1Token`
- MUST initialize ERC20 with `_name` and `_symbol`

### supportsInterface

Returns whether the contract supports a given interface ID per EIP165.

**Parameters:**

- `_interfaceId`: 4-byte interface identifier to check

**Behavior:**

- MUST return `true` if `_interfaceId` matches the ERC165 `supportsInterface(bytes4)` selector
- MUST return `true` if `_interfaceId` matches the XOR of `ILegacyMintableERC20.l1Token.selector`,
  `ILegacyMintableERC20.mint.selector`, and `ILegacyMintableERC20.burn.selector`
- MUST return `false` otherwise

### mint

Mints tokens to a specified address. Only callable by the L2 bridge.

**Parameters:**

- `_to`: Address to receive the minted tokens
- `_amount`: Amount of tokens to mint

**Behavior:**

- MUST revert if `msg.sender` is not `l2Bridge`
- MUST mint `_amount` tokens to `_to`
- MUST emit `Mint` event with `_to` and `_amount`

### burn

Burns tokens from a specified address. Only callable by the L2 bridge.

**Parameters:**

- `_from`: Address from which tokens will be burned
- `_amount`: Amount of tokens to burn

**Behavior:**

- MUST revert if `msg.sender` is not `l2Bridge`
- MUST burn `_amount` tokens from `_from`
- MUST emit `Burn` event with `_from` and `_amount`
