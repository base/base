# L1StandardBridge

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Native L1 Token](#native-l1-token)
- [Assumptions](#assumptions)
  - [a01-001: L2StandardBridge is trusted](#a01-001-l2standardbridge-is-trusted)
    - [Mitigations](#mitigations)
  - [a01-002: L1CrossDomainMessenger is trusted](#a01-002-l1crossdomainmessenger-is-trusted)
    - [Mitigations](#mitigations-1)
  - [a01-003: SystemConfig and SuperchainConfig are trusted](#a01-003-systemconfig-and-superchainconfig-are-trusted)
    - [Mitigations](#mitigations-2)
  - [a01-004: OptimismMintableERC20 tokens correctly implement mint and burn](#a01-004-optimismmintableerc20-tokens-correctly-implement-mint-and-burn)
    - [Mitigations](#mitigations-3)
- [Invariants](#invariants)
  - [i01-001: Users who deposit assets on L1 receive them on L2](#i01-001-users-who-deposit-assets-on-l1-receive-them-on-l2)
    - [Impact](#impact)
  - [i01-002: Users who withdraw assets from L2 can claim them on L1](#i01-002-users-who-withdraw-assets-from-l2-can-claim-them-on-l1)
    - [Impact](#impact-1)
  - [i01-003: One-to-one custody across chains](#i01-003-one-to-one-custody-across-chains)
    - [Impact](#impact-2)
- [Function Specification](#function-specification)
  - [initialize](#initialize)
  - [receive](#receive)
  - [depositETH](#depositeth)
  - [depositETHTo](#depositethto)
  - [depositERC20](#depositerc20)
  - [depositERC20To](#depositerc20to)
  - [bridgeETH](#bridgeeth)
  - [bridgeETHTo](#bridgeethto)
  - [bridgeERC20](#bridgeerc20)
  - [bridgeERC20To](#bridgeerc20to)
  - [finalizeETHWithdrawal](#finalizeethwithdrawal)
  - [finalizeERC20Withdrawal](#finalizeerc20withdrawal)
  - [finalizeBridgeETH](#finalizebridgeeth)
  - [finalizeBridgeERC20](#finalizebridgeerc20)
  - [l2TokenBridge](#l2tokenbridge)
  - [paused](#paused)
  - [superchainConfig](#superchainconfig)
  - [MESSENGER](#messenger)
  - [OTHER_BRIDGE](#other_bridge)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The L1StandardBridge enables ETH and ERC20 token transfers between Ethereum L1 and an OP Stack L2 chain. It escrows
L1-native tokens when depositing to L2 and releases them when finalizing withdrawals from L2. For L2-native tokens, it
mints them when finalizing withdrawals and burns them when depositing to L2.

## Definitions

### Native L1 Token

An ERC20 token that is native to the L1 chain and does not implement the
[OptimismMintableERC20](../L2/l2-standard-bridge.md#optimismmintableerc20) interface. These tokens are escrowed in
the bridge contract when deposited to L2 and released when withdrawals are finalized from L2.

## Assumptions

### a01-001: L2StandardBridge is trusted

The L2StandardBridge contract on L2 is trusted to only send valid finalization messages for legitimate withdrawals.

#### Mitigations

- The L2StandardBridge address is set during initialization and cannot be changed
- Cross-chain messages are authenticated through the L1CrossDomainMessenger

### a01-002: L1CrossDomainMessenger is trusted

The L1CrossDomainMessenger accurately validates cross-chain messages and correctly identifies the sender.

#### Mitigations

- The messenger is set during initialization and cannot be changed
- Message authentication uses the xDomainMessageSender mechanism

### a01-003: SystemConfig and SuperchainConfig are trusted

The SystemConfig and SuperchainConfig contracts accurately report the paused state and are controlled by trusted
governance.

#### Mitigations

- The SystemConfig address is set during initialization by the ProxyAdmin or its owner
- The pause mechanism provides emergency protection against detected issues

### a01-004: OptimismMintableERC20 tokens correctly implement mint and burn

[OptimismMintableERC20](../L2/l2-standard-bridge.md#optimismmintableerc20) tokens correctly implement the mint and burn
functions and accurately report their remote token address.

#### Mitigations

- Token pair validation checks that the remote token matches the token's reported remoteToken or l1Token
- Tokens must implement the IOptimismMintableERC20 or ILegacyMintableERC20 interface

## Invariants

### i01-001: Users who deposit assets on L1 receive them on L2

Users who successfully deposit ETH or ERC20 tokens through the L1 bridge always receive the corresponding assets on L2
when the deposit is finalized within a bounded time period (less than 1 day under normal operation).

#### Impact

**Severity: Critical**

If violated, users lose their assets permanently as the L1 assets are locked or burned but the L2 assets are never
received.

### i01-002: Users who withdraw assets from L2 can claim them on L1

Users who successfully initiate a withdrawal on L2 can always claim the corresponding assets on L1 after the challenge
period, provided the withdrawal is properly finalized on L1.

#### Impact

**Severity: Critical**

If violated, users lose their assets permanently as the L2 assets are burned or escrowed but the L1 assets cannot be
claimed.

### i01-003: One-to-one custody across chains

For each bridged asset amount, exactly one of the following holds at any time: it is held on L1 by the bridge (locked
or in original form) or it is represented on L2 (minted or in original form). Transitions between these states occur
only through the canonical flows: a valid deposit locks or burns the asset on L1 and is paired with a mint or release
on L2; a valid withdrawal burns or escrows the asset on L2 and is paired with a release or mint on L1. No other
mechanism may create or release representations.

#### Impact

**Severity: Critical**

If violated, attackers could mint arbitrary tokens on L2 without depositing the corresponding L1 tokens, or drain the
bridge by withdrawing more than was deposited, breaking the 1:1 correspondence between L1 and L2 assets.

## Function Specification

### initialize

Initializes the L1StandardBridge contract with the messenger and system configuration.

**Parameters:**

- `_messenger`: Address of the L1CrossDomainMessenger contract
- `_systemConfig`: Address of the SystemConfig contract

**Behavior:**

- MUST revert if caller is not the ProxyAdmin or ProxyAdmin owner
- MUST set the messenger to the provided L1CrossDomainMessenger address
- MUST set the systemConfig to the provided SystemConfig address
- MUST set the otherBridge to the L2StandardBridge predeploy at 0x4200000000000000000000000000000000000010
- MUST revert if called more than once

### receive

Allows EOAs to bridge ETH by sending directly to the bridge contract.

**Behavior:**

- MUST revert if caller is not an externally owned account
- MUST initiate an ETH deposit to the sender's address on L2
- MUST use RECEIVE_DEFAULT_GAS_LIMIT (200,000) as the minimum gas limit
- MUST emit ETHDepositInitiated and ETHBridgeInitiated events

### depositETH

Deposits ETH into the sender's account on L2.

**Parameters:**

- `_minGasLimit`: Minimum gas limit for the deposit message on L2
- `_extraData`: Optional data forwarded to L2 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not an externally owned account
- MUST revert if msg.value does not equal the amount being deposited
- MUST send a cross-chain message to L2StandardBridge calling finalizeBridgeETH
- MUST emit ETHDepositInitiated and ETHBridgeInitiated events

### depositETHTo

Deposits ETH into a target account on L2.

**Parameters:**

- `_to`: Address to receive the ETH on L2
- `_minGasLimit`: Minimum gas limit for the deposit message on L2
- `_extraData`: Optional data forwarded to L2 (not used for execution, only emitted)

**Behavior:**

- MUST revert if msg.value does not equal the amount being deposited
- MUST send a cross-chain message to L2StandardBridge calling finalizeBridgeETH
- MUST emit ETHDepositInitiated and ETHBridgeInitiated events

### depositERC20

Deposits ERC20 tokens into the sender's account on L2.

**Parameters:**

- `_l1Token`: Address of the L1 token being deposited
- `_l2Token`: Address of the corresponding token on L2
- `_amount`: Amount of the ERC20 to deposit
- `_minGasLimit`: Minimum gas limit for the deposit message on L2
- `_extraData`: Optional data forwarded to L2 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not an externally owned account
- MUST revert if msg.value is not zero
- MUST escrow the tokens if `_l1Token` is a Native L1 Token
- MUST burn the tokens if `_l1Token` is an OptimismMintableERC20
- MUST verify token pair matches for OptimismMintableERC20 tokens
- MUST send a cross-chain message to L2StandardBridge calling finalizeBridgeERC20
- MUST emit ERC20DepositInitiated and ERC20BridgeInitiated events

### depositERC20To

Deposits ERC20 tokens into a target account on L2.

**Parameters:**

- `_l1Token`: Address of the L1 token being deposited
- `_l2Token`: Address of the corresponding token on L2
- `_to`: Address to receive the tokens on L2
- `_amount`: Amount of the ERC20 to deposit
- `_minGasLimit`: Minimum gas limit for the deposit message on L2
- `_extraData`: Optional data forwarded to L2 (not used for execution, only emitted)

**Behavior:**

- MUST revert if msg.value is not zero
- MUST escrow the tokens if `_l1Token` is a Native L1 Token
- MUST burn the tokens if `_l1Token` is an OptimismMintableERC20
- MUST verify token pair matches for OptimismMintableERC20 tokens
- MUST send a cross-chain message to L2StandardBridge calling finalizeBridgeERC20
- MUST emit ERC20DepositInitiated and ERC20BridgeInitiated events

### bridgeETH

Sends ETH to the sender's address on L2.

**Parameters:**

- `_minGasLimit`: Minimum gas limit for the bridge message on L2
- `_extraData`: Optional data forwarded to L2 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not an externally owned account
- MUST revert if msg.value does not equal the amount being bridged
- MUST send a cross-chain message to L2StandardBridge calling finalizeBridgeETH
- MUST emit ETHDepositInitiated and ETHBridgeInitiated events

### bridgeETHTo

Sends ETH to a specified recipient's address on L2.

**Parameters:**

- `_to`: Address to receive the ETH on L2
- `_minGasLimit`: Minimum gas limit for the bridge message on L2
- `_extraData`: Optional data forwarded to L2 (not used for execution, only emitted)

**Behavior:**

- MUST revert if msg.value does not equal the amount being bridged
- MUST send a cross-chain message to L2StandardBridge calling finalizeBridgeETH
- MUST emit ETHDepositInitiated and ETHBridgeInitiated events

### bridgeERC20

Sends ERC20 tokens to the sender's address on L2.

**Parameters:**

- `_localToken`: Address of the ERC20 token on L1
- `_remoteToken`: Address of the corresponding token on L2
- `_amount`: Amount of tokens to bridge
- `_minGasLimit`: Minimum gas limit for the bridge message on L2
- `_extraData`: Optional data forwarded to L2 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not an externally owned account
- MUST revert if msg.value is not zero
- MUST escrow the tokens if `_localToken` is a Native L1 Token
- MUST burn the tokens if `_localToken` is an OptimismMintableERC20
- MUST verify token pair matches for OptimismMintableERC20 tokens
- MUST send a cross-chain message to L2StandardBridge calling finalizeBridgeERC20
- MUST emit ERC20DepositInitiated and ERC20BridgeInitiated events

### bridgeERC20To

Sends ERC20 tokens to a specified recipient's address on L2.

**Parameters:**

- `_localToken`: Address of the ERC20 token on L1
- `_remoteToken`: Address of the corresponding token on L2
- `_to`: Address to receive the tokens on L2
- `_amount`: Amount of tokens to bridge
- `_minGasLimit`: Minimum gas limit for the bridge message on L2
- `_extraData`: Optional data forwarded to L2 (not used for execution, only emitted)

**Behavior:**

- MUST revert if msg.value is not zero
- MUST escrow the tokens if `_localToken` is a Native L1 Token
- MUST burn the tokens if `_localToken` is an OptimismMintableERC20
- MUST verify token pair matches for OptimismMintableERC20 tokens
- MUST send a cross-chain message to L2StandardBridge calling finalizeBridgeERC20
- MUST emit ERC20DepositInitiated and ERC20BridgeInitiated events

### finalizeETHWithdrawal

Finalizes a withdrawal of ETH from L2.

**Parameters:**

- `_from`: Address of the withdrawer on L2
- `_to`: Address of the recipient on L1
- `_amount`: Amount of ETH to withdraw
- `_extraData`: Optional data forwarded from L2 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not the L1CrossDomainMessenger
- MUST revert if xDomainMessageSender is not the L2StandardBridge
- MUST revert if the bridge is paused
- MUST revert if msg.value does not equal `_amount`
- MUST revert if `_to` is the bridge contract itself
- MUST revert if `_to` is the messenger contract
- MUST send `_amount` of ETH to `_to`
- MUST revert if the ETH transfer fails
- MUST emit ETHWithdrawalFinalized and ETHBridgeFinalized events

### finalizeERC20Withdrawal

Finalizes a withdrawal of ERC20 tokens from L2.

**Parameters:**

- `_l1Token`: Address of the token on L1
- `_l2Token`: Address of the corresponding token on L2
- `_from`: Address of the withdrawer on L2
- `_to`: Address of the recipient on L1
- `_amount`: Amount of the ERC20 to withdraw
- `_extraData`: Optional data forwarded from L2 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not the L1CrossDomainMessenger
- MUST revert if xDomainMessageSender is not the L2StandardBridge
- MUST revert if the bridge is paused
- MUST mint tokens to `_to` if `_l1Token` is an OptimismMintableERC20
- MUST release escrowed tokens to `_to` if `_l1Token` is a Native L1 Token
- MUST verify token pair matches for OptimismMintableERC20 tokens
- MUST decrease the deposits mapping for Native L1 Tokens
- MUST emit ERC20WithdrawalFinalized and ERC20BridgeFinalized events

### finalizeBridgeETH

Completes an ETH bridge from L2 and sends ETH to the recipient on L1.

**Parameters:**

- `_from`: Address that initiated the bridge on L2
- `_to`: Address to receive the ETH on L1
- `_amount`: Amount of ETH being withdrawn
- `_extraData`: Optional data forwarded from L2 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not the L1CrossDomainMessenger
- MUST revert if xDomainMessageSender is not the L2StandardBridge
- MUST revert if the bridge is paused
- MUST revert if msg.value does not equal `_amount`
- MUST revert if `_to` is the bridge contract itself
- MUST revert if `_to` is the messenger contract
- MUST send `_amount` of ETH to `_to`
- MUST revert if the ETH transfer fails
- MUST emit ETHWithdrawalFinalized and ETHBridgeFinalized events

### finalizeBridgeERC20

Completes an ERC20 bridge from L2 and mints or releases tokens to the recipient on L1.

**Parameters:**

- `_localToken`: Address of the ERC20 token on L1
- `_remoteToken`: Address of the corresponding token on L2
- `_from`: Address that initiated the bridge on L2
- `_to`: Address to receive the tokens on L1
- `_amount`: Amount of tokens being withdrawn
- `_extraData`: Optional data forwarded from L2 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not the L1CrossDomainMessenger
- MUST revert if xDomainMessageSender is not the L2StandardBridge
- MUST revert if the bridge is paused
- MUST mint tokens to `_to` if `_localToken` is an OptimismMintableERC20
- MUST release escrowed tokens to `_to` if `_localToken` is a Native L1 Token
- MUST verify token pair matches for OptimismMintableERC20 tokens
- MUST decrease the deposits mapping for Native L1 Tokens
- MUST emit ERC20WithdrawalFinalized and ERC20BridgeFinalized events

### l2TokenBridge

Returns the address of the L2StandardBridge contract.

**Behavior:**

- MUST return the address of the otherBridge

### paused

Returns whether the bridge is paused.

**Behavior:**

- MUST return the paused status from the SystemConfig contract

### superchainConfig

Returns the SuperchainConfig contract.

**Behavior:**

- MUST return the SuperchainConfig contract from the SystemConfig

### MESSENGER

Legacy getter for the messenger contract.

**Behavior:**

- MUST return the L1CrossDomainMessenger contract address

### OTHER_BRIDGE

Legacy getter for the other bridge contract.

**Behavior:**

- MUST return the L2StandardBridge contract address
