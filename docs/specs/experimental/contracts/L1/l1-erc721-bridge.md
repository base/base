# L1ERC721Bridge

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Bridge Pair](#bridge-pair)
  - [Correct Deposit](#correct-deposit)
  - [Eligible Withdrawal](#eligible-withdrawal)
  - [Bridge Flow](#bridge-flow)
- [Assumptions](#assumptions)
  - [a01-001: CrossDomainMessenger delivers messages reliably](#a01-001-crossdomainmessenger-delivers-messages-reliably)
    - [Mitigations](#mitigations)
  - [a01-002: L2ERC721Bridge validates withdrawals correctly](#a01-002-l2erc721bridge-validates-withdrawals-correctly)
    - [Mitigations](#mitigations-1)
  - [a01-003: ProxyAdmin owner operates within governance constraints](#a01-003-proxyadmin-owner-operates-within-governance-constraints)
    - [Mitigations](#mitigations-2)
- [Invariants](#invariants)
  - [i01-001: L2 delivery for Correct Deposits](#i01-001-l2-delivery-for-correct-deposits)
    - [Impact](#impact)
  - [i01-002: L1 delivery for Eligible Withdrawals](#i01-002-l1-delivery-for-eligible-withdrawals)
    - [Impact](#impact-1)
  - [i01-003: No bypass of the Bridge Flow](#i01-003-no-bypass-of-the-bridge-flow)
    - [Impact](#impact-2)
- [Function Specification](#function-specification)
  - [initialize](#initialize)
  - [bridgeERC721](#bridgeerc721)
  - [bridgeERC721To](#bridgeerc721to)
  - [finalizeBridgeERC721](#finalizebridgeerc721)
  - [paused](#paused)
  - [superchainConfig](#superchainconfig)
  - [proxyAdmin](#proxyadmin)
  - [proxyAdminOwner](#proxyadminowner)
  - [MESSENGER](#messenger)
  - [OTHER_BRIDGE](#other_bridge)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The L1ERC721Bridge enables trustless transfer of ERC721 tokens between Ethereum L1 and Optimism L2 by escrowing
tokens on L1 during deposits and releasing them upon withdrawal finalization.

## Definitions

### Bridge Pair

A specific combination of L1 token address and L2 token address that represents the same logical ERC721 collection
across both chains.

### Correct Deposit

A successful deposit initiation on L1 via bridgeERC721 or bridgeERC721To that meets all function preconditions and
completes, resulting in a message to L2 for the specified [Bridge Pair](#bridge-pair) and token ID.

### Eligible Withdrawal

A withdrawal initiated by the L2ERC721Bridge that corresponds to an escrowed token and whose message has been
delivered to L1 and is eligible for finalization according to the bridge protocol.

### Bridge Flow

The deposit and withdrawal processes mediated by the CrossDomainMessenger between the L1 and L2 ERC721 bridges.

## Assumptions

### a01-001: CrossDomainMessenger delivers messages reliably

The CrossDomainMessenger contract correctly delivers cross-chain messages between L1 and L2 without loss or
corruption.

#### Mitigations

- CrossDomainMessenger is a core protocol contract with extensive testing and auditing
- Message delivery is secured by L1 data availability and fault proof system

### a01-002: L2ERC721Bridge validates withdrawals correctly

The L2ERC721Bridge contract on L2 only initiates withdrawal messages for tokens that were legitimately deposited
from L1.

#### Mitigations

- L2ERC721Bridge is a protocol-controlled predeploy contract
- Withdrawal validation logic is part of the core protocol specification

### a01-003: ProxyAdmin owner operates within governance constraints

The ProxyAdmin owner (governance) acts honestly when initializing or upgrading the contract.

#### Mitigations

- ProxyAdmin owner is typically a multisig or governance contract
- Upgrades follow established governance processes with time delays

## Invariants

### i01-001: L2 delivery for Correct Deposits

Any [Correct Deposit](#correct-deposit) MUST eventually result, after the corresponding message is delivered and
processed on L2, in
the designated recipient owning the same token ID of the corresponding remote token for the same [Bridge
Pair](#bridge-pair).

#### Impact

**Severity: High**

If violated, users can lose access to their asset (locked on L1 without L2 delivery), creating stuck funds and loss
of availability. This liveness property depends on correct message delivery and the L2 bridge's behavior.

### i01-002: L1 delivery for Eligible Withdrawals

Any [Eligible Withdrawal](#eligible-withdrawal) MUST be able to be finalized on L1 when the bridge is not paused,
resulting in the
designated recipient receiving the exact token ID of the local token for the same [Bridge Pair](#bridge-pair).

#### Impact

**Severity: High**

If violated, users cannot reclaim assets that were withdrawn on L2, leading to stuck assets in escrow. This liveness
property depends on correct message delivery and protocol withdrawal eligibility.

### i01-003: No bypass of the Bridge Flow

It MUST be impossible for any address to receive an ERC721 token via the bridge on either chain except through the
[Bridge Flow](#bridge-flow). Ownership changes across chains MUST only occur via deposits (L1→L2) and withdrawals
(L2→L1)
processed through the respective bridges and the CrossDomainMessenger.

#### Impact

**Severity: Critical**

If violated, an attacker could obtain assets without performing a valid deposit/withdrawal, enabling theft or
duplication independent of external assumptions.

## Function Specification

### initialize

Initializes the L1ERC721Bridge contract with the CrossDomainMessenger and SystemConfig addresses.

**Parameters:**

- `_messenger`: Address of the CrossDomainMessenger contract on L1
- `_systemConfig`: Address of the SystemConfig contract on L1

**Behavior:**

- MUST revert if caller is not the ProxyAdmin or ProxyAdmin owner
- MUST revert if already initialized at the current initialization version
- MUST set the systemConfig state variable to `_systemConfig`
- MUST set the messenger state variable to `_messenger`
- MUST set the otherBridge state variable to the L2ERC721Bridge predeploy address
  (0x4200000000000000000000000000000000000014)
- MUST emit an Initialized event

### bridgeERC721

Initiates a bridge transfer of an ERC721 token to the caller's address on L2.

**Parameters:**

- `_localToken`: Address of the ERC721 token contract on L1
- `_remoteToken`: Address of the corresponding ERC721 token contract on L2
- `_tokenId`: Token ID to bridge
- `_minGasLimit`: Minimum gas limit for the L2 transaction
- `_extraData`: Optional additional data forwarded to L2 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not an externally owned account (EOA)
- MUST revert if `_remoteToken` is address(0)
- MUST revert if the caller has not approved this contract to transfer the token
- MUST set `deposits[_localToken][_remoteToken][_tokenId]` to true
- MUST transfer the token from caller to this contract using transferFrom
- MUST send a cross-chain message to L2ERC721Bridge calling finalizeBridgeERC721
- MUST emit ERC721BridgeInitiated event with caller as both _from and_to addresses

### bridgeERC721To

Initiates a bridge transfer of an ERC721 token to a specified recipient address on L2.

**Parameters:**

- `_localToken`: Address of the ERC721 token contract on L1
- `_remoteToken`: Address of the corresponding ERC721 token contract on L2
- `_to`: Address to receive the token on L2
- `_tokenId`: Token ID to bridge
- `_minGasLimit`: Minimum gas limit for the L2 transaction
- `_extraData`: Optional additional data forwarded to L2 (not used for execution, only emitted)

**Behavior:**

- MUST revert if `_to` is address(0)
- MUST revert if `_remoteToken` is address(0)
- MUST revert if the caller has not approved this contract to transfer the token
- MUST set `deposits[_localToken][_remoteToken][_tokenId]` to true
- MUST transfer the token from caller to this contract using transferFrom
- MUST send a cross-chain message to L2ERC721Bridge calling finalizeBridgeERC721 with `_to` as recipient
- MUST emit ERC721BridgeInitiated event with caller as _from and `_to` as recipient

### finalizeBridgeERC721

Completes an ERC721 bridge withdrawal from L2 by releasing the escrowed token to the recipient on L1.

**Parameters:**

- `_localToken`: Address of the ERC721 token contract on L1
- `_remoteToken`: Address of the ERC721 token contract on L2
- `_from`: Address that initiated the withdrawal on L2
- `_to`: Address to receive the token on L1
- `_tokenId`: Token ID being withdrawn
- `_extraData`: Optional additional data (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not the CrossDomainMessenger
- MUST revert if the CrossDomainMessenger's xDomainMessageSender is not the L2ERC721Bridge
- MUST revert if the contract is paused
- MUST revert if `_localToken` is the address of this contract
- MUST revert if `deposits[_localToken][_remoteToken][_tokenId]` is false
- MUST set `deposits[_localToken][_remoteToken][_tokenId]` to false
- MUST transfer the token from this contract to `_to` using safeTransferFrom
- MUST emit ERC721BridgeFinalized event

### paused

Returns whether the bridge is currently paused.

**Behavior:**

- MUST return the paused status from the SystemConfig's SuperchainConfig contract
- MUST NOT revert under any circumstances

### superchainConfig

Returns the SuperchainConfig contract address.

**Behavior:**

- MUST return the SuperchainConfig address from the SystemConfig contract
- MUST NOT revert under any circumstances

### proxyAdmin

Returns the ProxyAdmin contract that controls this proxy.

**Behavior:**

- MUST return the ProxyAdmin contract address that controls this proxy
- MUST revert if no ProxyAdmin is discoverable

### proxyAdminOwner

Returns the owner of the ProxyAdmin contract.

**Behavior:**

- MUST return the owner address of the ProxyAdmin that controls this proxy
- MUST revert if the ProxyAdmin cannot be determined

### MESSENGER

Legacy getter for the messenger contract address.

**Behavior:**

- MUST return the messenger state variable
- MUST NOT revert under any circumstances

### OTHER_BRIDGE

Legacy getter for the other bridge contract address.

**Behavior:**

- MUST return the otherBridge state variable
- MUST NOT revert under any circumstances
