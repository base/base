# OptimismMintableERC721Factory

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Local Token](#local-token)
- [Assumptions](#assumptions)
  - [a01-001: Bridge Contract Integrity](#a01-001-bridge-contract-integrity)
    - [Mitigations](#mitigations)
- [Invariants](#invariants)
  - [i01-001: Permissionless deployment](#i01-001-permissionless-deployment)
    - [Impact](#impact)
  - [i01-002: Deterministic uniqueness for identical parameters](#i01-002-deterministic-uniqueness-for-identical-parameters)
    - [Impact](#impact-1)
- [Function Specification](#function-specification)
  - [constructor](#constructor)
  - [bridge](#bridge)
  - [remoteChainID](#remotechainid)
  - [createOptimismMintableERC721](#createoptimismmintableerc721)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

Factory contract for deploying L2 ERC721 tokens that represent L1 NFTs in the OP Stack bridge system.

## Definitions

### Local Token

An ERC721 token deployed on L2 by this factory that represents a
[remote token](optimism-mintable-erc721.md#remote-token) from another chain.

## Assumptions

### a01-001: Bridge Contract Integrity

The bridge address provided at deployment is a trusted ERC721 bridge contract that will properly manage minting
and burning of tokens created by this factory.

#### Mitigations

- Bridge address is immutable and set at deployment
- Factory is typically deployed as a predeploy with governance-controlled configuration

## Invariants

### i01-001: Permissionless deployment

Any address can deploy a [Local Token] via this factory. The factory does not restrict the caller for
deployments.

#### Impact

**Severity: Low**

If this property is broken, token deployment becomes permissioned, reducing openness and changing expected user
experience.

### i01-002: Deterministic uniqueness for identical parameters

For any tuple (Remote Token, name, symbol), at most one [Local Token] exists. Re-invoking deployment with
identical parameters does not result in an additional token contract.

#### Impact

**Severity: Medium**

If this property is broken, multiple distinct contracts could represent the same remote token and metadata,
leading to user confusion and fragmented integrations.

## Function Specification

### constructor

Initializes the factory with bridge and remote chain configuration.

**Parameters:**

- `_bridge`: Address of the ERC721 bridge on this network
- `_remoteChainId`: Chain ID for the remote network

**Behavior:**

- MUST set `BRIDGE` to `_bridge`
- MUST set `REMOTE_CHAIN_ID` to `_remoteChainId`

### bridge

Returns the address of the ERC721 bridge on this network.

**Behavior:**

- MUST return the `BRIDGE` immutable value

### remoteChainID

Returns the chain ID for the remote network.

**Behavior:**

- MUST return the `REMOTE_CHAIN_ID` immutable value

### createOptimismMintableERC721

Creates a new [OptimismMintableERC721](optimism-mintable-erc721.md) token contract that represents a remote token.

**Parameters:**

- `_remoteToken`: Address of the corresponding token on the remote chain
- `_name`: ERC721 name for the new token
- `_symbol`: ERC721 symbol for the new token

**Behavior:**

- MUST be callable by any address without access restrictions
- MUST revert if `_remoteToken` is the zero address
- MUST deterministically compute a deployment address from `_remoteToken`, `_name`, and `_symbol`
- MUST revert if a contract already exists at the computed deployment address, preventing duplicate deployments
  with identical parameters
- MUST compute a salt as `keccak256(abi.encode(_remoteToken, _name, _symbol))`
- MUST deploy a new OptimismMintableERC721 contract using CREATE2 with the computed salt
- MUST pass `BRIDGE`, `REMOTE_CHAIN_ID`, `_remoteToken`, `_name`, and `_symbol` to the token constructor
- MUST set `isOptimismMintableERC721[localToken]` to `true` for the deployed token address
- MUST emit `OptimismMintableERC721Created` event with the local token address, remote token address, and
  `msg.sender` as the deployer
- MUST return the address of the deployed token
