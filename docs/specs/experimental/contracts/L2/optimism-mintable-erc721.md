# OptimismMintableERC721

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Remote Token](#remote-token)
  - [Remote Chain](#remote-chain)
- [Assumptions](#assumptions)
  - [a01-001: Bridge contract is trusted](#a01-001-bridge-contract-is-trusted)
    - [Mitigations](#mitigations)
- [Invariants](#invariants)
- [Function Specification](#function-specification)
  - [constructor](#constructor)
  - [remoteChainId](#remotechainid)
  - [remoteToken](#remotetoken)
  - [bridge](#bridge)
  - [safeMint](#safemint)
  - [burn](#burn)
  - [supportsInterface](#supportsinterface)
  - [_baseURI](#_baseuri)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The OptimismMintableERC721 contract represents an ERC721 token on L2 that corresponds to a native ERC721 token on
another chain (typically L1). It enables cross-chain NFT transfers through the ERC721 bridge by allowing the bridge to
mint tokens when NFTs are deposited and burn tokens when NFTs are withdrawn.

## Definitions

### Remote Token

The address of the corresponding ERC721 token contract on the remote chain (typically L1) that this L2 token
represents.

### Remote Chain

The chain where the [Remote Token](#remote-token) is natively deployed, identified by its chain ID.

## Assumptions

### a01-001: Bridge contract is trusted

The bridge contract specified at deployment is trusted to only mint tokens corresponding to valid deposits from the
[Remote Chain](#remote-chain) and only burn tokens for valid withdrawal requests.

#### Mitigations

- Bridge address is immutable and set at deployment
- Bridge contract is part of the audited OP Stack protocol

## Invariants

N/A - This contract has no non-obvious cross-function invariants. All critical properties are enforced through
single-function access control and standard ERC721 behavior.

## Function Specification

### constructor

Initializes the OptimismMintableERC721 contract with bridge configuration and token metadata.

**Parameters:**

- `_bridge`: Address of the ERC721 bridge contract on this network
- `_remoteChainId`: Chain ID where the [Remote Token](#remote-token) is deployed
- `_remoteToken`: Address of the corresponding token on the [Remote Chain](#remote-chain)
- `_name`: ERC721 token name
- `_symbol`: ERC721 token symbol

**Behavior:**

- MUST revert if `_bridge` is the zero address
- MUST revert if `_remoteChainId` is zero
- MUST revert if `_remoteToken` is the zero address
- MUST set `BRIDGE` to `_bridge`
- MUST set `REMOTE_CHAIN_ID` to `_remoteChainId`
- MUST set `REMOTE_TOKEN` to `_remoteToken`
- MUST set `baseTokenURI` to an EIP-681 formatted URI: `ethereum:{_remoteToken}@{_remoteChainId}/tokenURI?uint256=`
- MUST initialize the ERC721 contract with `_name` and `_symbol`

### remoteChainId

Returns the chain ID where the [Remote Token](#remote-token) is deployed.

**Behavior:**

- MUST return the value of `REMOTE_CHAIN_ID`

### remoteToken

Returns the address of the token on the [Remote Chain](#remote-chain).

**Behavior:**

- MUST return the value of `REMOTE_TOKEN`

### bridge

Returns the address of the ERC721 bridge on this network.

**Behavior:**

- MUST return the value of `BRIDGE`

### safeMint

Mints a token to a recipient address, checking that contract recipients implement the ERC721 receiver interface.

**Parameters:**

- `_to`: Address to mint the token to
- `_tokenId`: Token ID to mint

**Behavior:**

- MUST revert if caller is not the bridge contract
- MUST revert if `_to` is the zero address
- MUST revert if `_tokenId` already exists
- MUST revert if `_to` is a contract that does not implement IERC721Receiver
- MUST mint `_tokenId` to `_to`
- MUST emit `Mint` event with `_to` and `_tokenId`
- MUST emit standard ERC721 `Transfer` event from zero address to `_to` with `_tokenId`

### burn

Burns a token from an owner address.

**Parameters:**

- `_from`: Address to burn the token from (informational, not used for validation)
- `_tokenId`: Token ID to burn

**Behavior:**

- MUST revert if caller is not the bridge contract
- MUST revert if `_tokenId` does not exist
- MUST burn `_tokenId` from its current owner
- MUST emit `Burn` event with `_from` and `_tokenId`
- MUST emit standard ERC721 `Transfer` event from current owner to zero address with `_tokenId`

### supportsInterface

Checks if the contract implements a given interface.

**Parameters:**

- `_interfaceId`: The interface identifier to check

**Behavior:**

- MUST return true if `_interfaceId` matches the IOptimismMintableERC721 interface ID
- MUST return true if `_interfaceId` matches any interface supported by ERC721Enumerable
- MUST return false otherwise

### _baseURI

Returns the base URI for computing token URIs.

**Behavior:**

- MUST return the value of `baseTokenURI`
