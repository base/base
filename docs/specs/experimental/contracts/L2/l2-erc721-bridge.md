# L2ERC721Bridge

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [OptimismMintableERC721](#optimismmintableerc721)
- [Assumptions](#assumptions)
  - [a01-001: L1ERC721Bridge is trusted](#a01-001-l1erc721bridge-is-trusted)
    - [Mitigations](#mitigations)
  - [a01-002: L2CrossDomainMessenger is trusted](#a01-002-l2crossdomainmessenger-is-trusted)
    - [Mitigations](#mitigations-1)
- [Invariants](#invariants)
  - [i01-001: Users who deposit NFTs on L1 receive them on L2](#i01-001-users-who-deposit-nfts-on-l1-receive-them-on-l2)
    - [Impact](#impact)
  - [i01-002: Users who withdraw NFTs on L2 can claim them on L1](#i01-002-users-who-withdraw-nfts-on-l2-can-claim-them-on-l1)
    - [Impact](#impact-1)
  - [i01-003: One-to-one custody across chains via paired lock/mint and burn/unlock](#i01-003-one-to-one-custody-across-chains-via-paired-lockmint-and-burnunlock)
    - [Impact](#impact-2)
- [Function Specification](#function-specification)
  - [initialize](#initialize)
  - [finalizeBridgeERC721](#finalizebridgeerc721)
  - [bridgeERC721](#bridgeerc721)
  - [bridgeERC721To](#bridgeerc721to)
  - [_initiateBridgeERC721](#_initiatebridgeerc721)
  - [paused](#paused)
  - [MESSENGER](#messenger)
  - [OTHER_BRIDGE](#other_bridge)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The L2ERC721Bridge enables ERC721 token transfers between Ethereum L1 and an OP Stack L2 chain. It mints tokens
on L2 when deposits are finalized from L1 and burns tokens on L2 when withdrawals are initiated to L1.

## Definitions

### OptimismMintableERC721

An ERC721 token that implements the IOptimismMintableERC721 interface, allowing the bridge to mint and burn tokens.
These tokens must be originally deployed on L1 and have a corresponding L2 representation that can be minted/burned
by the bridge.

## Assumptions

### a01-001: L1ERC721Bridge is trusted

The L1ERC721Bridge contract on L1 is trusted to only send valid finalization messages for legitimate deposits.

#### Mitigations

- The L1ERC721Bridge address is set during initialization and cannot be changed
- Cross-chain messages are authenticated through the L2CrossDomainMessenger

### a01-002: L2CrossDomainMessenger is trusted

The L2CrossDomainMessenger accurately validates cross-chain messages and correctly identifies the sender.

#### Mitigations

- The messenger is a system predeploy contract maintained by the protocol
- Message authentication uses the xDomainMessageSender mechanism

## Invariants

### i01-001: Users who deposit NFTs on L1 receive them on L2

Users who successfully deposit an NFT through the L1 bridge always receive the corresponding NFT on L2 when the
deposit is finalized.

#### Impact

**Severity: Critical**

If violated, users lose their NFTs permanently as the L1 token is escrowed but the L2 token is never minted.

### i01-002: Users who withdraw NFTs on L2 can claim them on L1

Users who successfully initiate a withdrawal on L2 can always claim the corresponding NFT on L1 after the challenge
period, provided the withdrawal is properly finalized on L1.

#### Impact

**Severity: Critical**

If violated, users lose their NFTs permanently as the L2 token is burned but the L1 token cannot be claimed.

### i01-003: One-to-one custody across chains via paired lock/mint and burn/unlock

For each bridged tokenId, exactly one of the following holds at any time: it is held on L1 by the bridge
(locked) or it is represented on L2 by exactly one OptimismMintableERC721. Transitions between these states
occur only through the canonical flows: a valid deposit locks the token on L1 and is paired with a mint on L2;
a valid withdrawal burns the token on L2 and is paired with an unlock on L1. No other mechanism may create or
release representations.

#### Impact

**Severity: Critical**

If violated, attackers could mint arbitrary NFTs on L2 without depositing the corresponding L1 tokens, breaking the
1:1 correspondence between L1 and L2 tokens.

## Function Specification

### initialize

Initializes the L2ERC721Bridge contract with the L1 bridge address.

**Parameters:**

- `_l1ERC721Bridge`: Address of the L1ERC721Bridge contract

**Behavior:**

- MUST set the messenger to the L2CrossDomainMessenger predeploy at 0x4200000000000000000000000000000000000007
- MUST set the otherBridge to the provided L1ERC721Bridge address
- MUST revert if called more than once (initializer modifier)

### finalizeBridgeERC721

Completes an ERC721 bridge from L1 and mints the token to the recipient on L2.

**Parameters:**

- `_localToken`: Address of the OptimismMintableERC721 token on L2
- `_remoteToken`: Address of the ERC721 token on L1
- `_from`: Address that initiated the bridge on L1
- `_to`: Address to receive the token on L2
- `_tokenId`: ID of the token being deposited
- `_extraData`: Optional data forwarded from L1 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not the L2CrossDomainMessenger
- MUST revert if xDomainMessageSender is not the L1ERC721Bridge
- MUST revert if `_localToken` is the bridge contract itself
- MUST revert if `_localToken` does not implement IOptimismMintableERC721
- MUST revert if `_remoteToken` does not match the remoteToken() value of `_localToken`
- MUST call safeMint on `_localToken` to mint `_tokenId` to `_to`
- MUST emit ERC721BridgeFinalized event with all parameters

### bridgeERC721

Initiates a bridge of an NFT to the caller's account on L1.

**Parameters:**

- `_localToken`: Address of the OptimismMintableERC721 token on L2
- `_remoteToken`: Address of the ERC721 token on L1
- `_tokenId`: Token ID to bridge
- `_minGasLimit`: Minimum gas limit for the bridge message on L1
- `_extraData`: Optional data forwarded to L1 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not an externally owned account (EOA)
- MUST call _initiateBridgeERC721 with msg.sender as both `_from` and `_to`

### bridgeERC721To

Initiates a bridge of an NFT to a specified recipient's account on L1.

**Parameters:**

- `_localToken`: Address of the OptimismMintableERC721 token on L2
- `_remoteToken`: Address of the ERC721 token on L1
- `_to`: Address to receive the token on L1
- `_tokenId`: Token ID to bridge
- `_minGasLimit`: Minimum gas limit for the bridge message on L1
- `_extraData`: Optional data forwarded to L1 (not used for execution, only emitted)

**Behavior:**

- MUST revert if `_to` is address(0)
- MUST call _initiateBridgeERC721 with msg.sender as `_from` and provided `_to`

### _initiateBridgeERC721

Internal function that initiates a token bridge to L1.

**Parameters:**

- `_localToken`: Address of the OptimismMintableERC721 token on L2
- `_remoteToken`: Address of the ERC721 token on L1
- `_from`: Address of the sender on L2
- `_to`: Address to receive the token on L1
- `_tokenId`: Token ID to bridge
- `_minGasLimit`: Minimum gas limit for the bridge message on L1
- `_extraData`: Optional data forwarded to L1 (not used for execution, only emitted)

**Behavior:**

- MUST revert if `_remoteToken` is address(0)
- MUST revert if `_from` is not the owner of `_tokenId` in `_localToken`
- MUST retrieve remoteToken from `_localToken` and verify it matches `_remoteToken`
- MUST call burn on `_localToken` to burn `_tokenId` from `_from`
- MUST send a cross-chain message to L1ERC721Bridge calling finalizeBridgeERC721 with the parameters
- MUST emit ERC721BridgeInitiated event with all parameters

### paused

Returns whether the bridge is paused.

**Behavior:**

- MUST always return false (L2 bridge cannot be paused)

### MESSENGER

Legacy getter for the messenger contract.

**Behavior:**

- MUST return the messenger contract address

### OTHER_BRIDGE

Legacy getter for the other bridge contract.

**Behavior:**

- MUST return the L1ERC721Bridge contract address
