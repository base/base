# L2StandardBridge

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [OptimismMintableERC20](#optimismmintableerc20)
  - [Native L2 Token](#native-l2-token)
- [Assumptions](#assumptions)
  - [a01-001: L1StandardBridge is trusted](#a01-001-l1standardbridge-is-trusted)
    - [Mitigations](#mitigations)
  - [a01-002: L2CrossDomainMessenger is trusted](#a01-002-l2crossdomainmessenger-is-trusted)
    - [Mitigations](#mitigations-1)
  - [a01-003: OptimismMintableERC20 tokens correctly implement mint and burn](#a01-003-optimismmintableerc20-tokens-correctly-implement-mint-and-burn)
    - [Mitigations](#mitigations-2)
- [Invariants](#invariants)
  - [i01-001: Users who deposit assets on L1 receive them on L2](#i01-001-users-who-deposit-assets-on-l1-receive-them-on-l2)
    - [Impact](#impact)
  - [i01-002: Users who withdraw assets on L2 can claim them on L1](#i01-002-users-who-withdraw-assets-on-l2-can-claim-them-on-l1)
    - [Impact](#impact-1)
  - [i01-003: One-to-one custody across chains](#i01-003-one-to-one-custody-across-chains)
    - [Impact](#impact-2)
- [Function Specification](#function-specification)
  - [initialize](#initialize)
  - [receive](#receive)
  - [withdraw](#withdraw)
  - [withdrawTo](#withdrawto)
  - [bridgeETH](#bridgeeth)
  - [bridgeETHTo](#bridgeethto)
  - [bridgeERC20](#bridgeerc20)
  - [bridgeERC20To](#bridgeerc20to)
  - [finalizeBridgeETH](#finalizebridgeeth)
  - [finalizeBridgeERC20](#finalizebridgeerc20)
  - [l1TokenBridge](#l1tokenbridge)
  - [paused](#paused)
  - [MESSENGER](#messenger)
  - [OTHER_BRIDGE](#other_bridge)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The L2StandardBridge enables ETH and ERC20 token transfers between Ethereum L1 and an OP Stack L2 chain. It burns
L1-native tokens on L2 when withdrawing to L1 and mints them when finalizing deposits from L1. For L2-native tokens,
it escrows them when withdrawing to L1 and releases them when finalizing deposits from L1.

## Definitions

### OptimismMintableERC20

An ERC20 token that implements the IOptimismMintableERC20 or ILegacyMintableERC20 interface, allowing the bridge to
mint and burn tokens. These tokens are native to L1 and have a corresponding L2 representation that can be
minted/burned by the bridge.

### Native L2 Token

An ERC20 token that is native to the L2 chain and does not implement the OptimismMintableERC20 interface. These
tokens are escrowed in the bridge contract when withdrawn to L1 and released when deposits are finalized from L1.

## Assumptions

### a01-001: L1StandardBridge is trusted

The L1StandardBridge contract on L1 is trusted to only send valid finalization messages for legitimate deposits.

#### Mitigations

- The L1StandardBridge address is set during initialization and cannot be changed
- Cross-chain messages are authenticated through the L2CrossDomainMessenger

### a01-002: L2CrossDomainMessenger is trusted

The L2CrossDomainMessenger accurately validates cross-chain messages and correctly identifies the sender.

#### Mitigations

- The messenger is a system predeploy contract maintained by the protocol
- Message authentication uses the xDomainMessageSender mechanism

### a01-003: [OptimismMintableERC20](#optimismmintableerc20) tokens correctly implement mint and burn

[OptimismMintableERC20](#optimismmintableerc20) tokens correctly implement the mint and burn functions and accurately
report their remote
token address.

#### Mitigations

- Token pair validation checks that the remote token matches the token's reported remoteToken or l1Token
- Tokens must implement the I[OptimismMintableERC20](#optimismmintableerc20) or ILegacyMintableERC20 interface

## Invariants

### i01-001: Users who deposit assets on L1 receive them on L2

Users who successfully deposit ETH or ERC20 tokens through the L1 bridge always receive the corresponding assets on
L2 when the deposit is finalized within a bounded time period (less than 1 day under normal operation).

#### Impact

**Severity: Critical**

If violated, users lose their assets permanently as the L1 assets are locked or burned but the L2 assets are never
received.

### i01-002: Users who withdraw assets on L2 can claim them on L1

Users who successfully initiate a withdrawal on L2 can always claim the corresponding assets on L1 after the
challenge period, provided the withdrawal is properly finalized on L1.

#### Impact

**Severity: Critical**

If violated, users lose their assets permanently as the L2 assets are burned or escrowed but the L1 assets cannot
be claimed.

### i01-003: One-to-one custody across chains

For each bridged asset amount, exactly one of the following holds at any time: it is held on L1 by the bridge
(locked or in original form) or it is represented on L2 (minted or in original form). Transitions between these
states occur only through the canonical flows: a valid deposit locks or burns the asset on L1 and is paired with a
mint or release on L2; a valid withdrawal burns or escrows the asset on L2 and is paired with a release or mint on
L1. No other mechanism may create or release representations.

#### Impact

**Severity: Critical**

If violated, attackers could mint arbitrary tokens on L2 without depositing the corresponding L1 tokens, or drain
the bridge by withdrawing more than was deposited, breaking the 1:1 correspondence between L1 and L2 assets.

## Function Specification

### initialize

Initializes the L2StandardBridge contract with the L1 bridge address.

**Parameters:**

- `_otherBridge`: Address of the L1StandardBridge contract

**Behavior:**

- MUST set the messenger to the L2CrossDomainMessenger predeploy at 0x4200000000000000000000000000000000000007
- MUST set the otherBridge to the provided L1StandardBridge address
- MUST revert if called more than once

### receive

Allows EOAs to bridge ETH by sending directly to the bridge contract.

**Behavior:**

- MUST revert if caller is not an externally owned account
- MUST initiate an ETH withdrawal to the sender's address on L1
- MUST use RECEIVE_DEFAULT_GAS_LIMIT (200,000) as the minimum gas limit
- MUST emit WithdrawalInitiated and ETHBridgeInitiated events

### withdraw

Initiates a withdrawal from L2 to L1 for the caller.

**Parameters:**

- `_l2Token`: Address of the L2 token to withdraw (or 0xDeadDeAddeAddEAddeadDEaDDEAdDeaDDeAD0000 for ETH)
- `_amount`: Amount of the token to withdraw
- `_minGasLimit`: Minimum gas limit for the withdrawal message on L1
- `_extraData`: Optional data forwarded to L1 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not an externally owned account
- MUST call _initiateWithdrawal with msg.sender as both sender and recipient
- MUST emit WithdrawalInitiated and ETHBridgeInitiated or ERC20BridgeInitiated events

### withdrawTo

Initiates a withdrawal from L2 to L1 to a specified recipient on L1.

**Parameters:**

- `_l2Token`: Address of the L2 token to withdraw (or 0xDeadDeAddeAddEAddeadDEaDDEAdDeaDDeAD0000 for ETH)
- `_to`: Address to receive the assets on L1
- `_amount`: Amount of the token to withdraw
- `_minGasLimit`: Minimum gas limit for the withdrawal message on L1
- `_extraData`: Optional data forwarded to L1 (not used for execution, only emitted)

**Behavior:**

- MUST call `_initiateWithdrawal` with msg.sender as sender and provided `_to` as recipient
- MUST emit WithdrawalInitiated and ETHBridgeInitiated or ERC20BridgeInitiated events

### bridgeETH

Sends ETH to the sender's address on L1.

**Parameters:**

- `_minGasLimit`: Minimum gas limit for the bridge message on L1
- `_extraData`: Optional data forwarded to L1 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not an externally owned account
- MUST revert if msg.value does not equal the amount being bridged
- MUST send a cross-chain message to L1StandardBridge calling finalizeBridgeETH
- MUST emit WithdrawalInitiated and ETHBridgeInitiated events

### bridgeETHTo

Sends ETH to a specified recipient's address on L1.

**Parameters:**

- `_to`: Address to receive the ETH on L1
- `_minGasLimit`: Minimum gas limit for the bridge message on L1
- `_extraData`: Optional data forwarded to L1 (not used for execution, only emitted)

**Behavior:**

- MUST revert if msg.value does not equal the amount being bridged
- MUST send a cross-chain message to L1StandardBridge calling finalizeBridgeETH
- MUST emit WithdrawalInitiated and ETHBridgeInitiated events

### bridgeERC20

Sends ERC20 tokens to the sender's address on L1.

**Parameters:**

- `_localToken`: Address of the ERC20 token on L2
- `_remoteToken`: Address of the corresponding token on L1
- `_amount`: Amount of tokens to bridge
- `_minGasLimit`: Minimum gas limit for the bridge message on L1
- `_extraData`: Optional data forwarded to L1 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not an externally owned account
- MUST revert if msg.value is not zero
- MUST burn the tokens if _localToken is an [OptimismMintableERC20](#optimismmintableerc20)
- MUST escrow the tokens if _localToken is a [Native L2 Token](#native-l2-token)
- MUST verify token pair matches for [OptimismMintableERC20](#optimismmintableerc20) tokens
- MUST send a cross-chain message to L1StandardBridge calling finalizeBridgeERC20
- MUST emit WithdrawalInitiated and ERC20BridgeInitiated events

### bridgeERC20To

Sends ERC20 tokens to a specified recipient's address on L1.

**Parameters:**

- `_localToken`: Address of the ERC20 token on L2
- `_remoteToken`: Address of the corresponding token on L1
- `_to`: Address to receive the tokens on L1
- `_amount`: Amount of tokens to bridge
- `_minGasLimit`: Minimum gas limit for the bridge message on L1
- `_extraData`: Optional data forwarded to L1 (not used for execution, only emitted)

**Behavior:**

- MUST revert if msg.value is not zero
- MUST burn the tokens if _localToken is an [OptimismMintableERC20](#optimismmintableerc20)
- MUST escrow the tokens if _localToken is a [Native L2 Token](#native-l2-token)
- MUST verify token pair matches for [OptimismMintableERC20](#optimismmintableerc20) tokens
- MUST send a cross-chain message to L1StandardBridge calling finalizeBridgeERC20
- MUST emit WithdrawalInitiated and ERC20BridgeInitiated events

### finalizeBridgeETH

Completes an ETH bridge from L1 and sends ETH to the recipient on L2.

**Parameters:**

- `_from`: Address that initiated the bridge on L1
- `_to`: Address to receive the ETH on L2
- `_amount`: Amount of ETH being deposited
- `_extraData`: Optional data forwarded from L1 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not the L2CrossDomainMessenger
- MUST revert if xDomainMessageSender is not the L1StandardBridge
- MUST revert if the bridge is paused
- MUST revert if msg.value does not equal `_amount`
- MUST revert if `_to` is the bridge contract itself
- MUST revert if `_to` is the messenger contract
- MUST send `_amount` of ETH to `_to`
- MUST revert if the ETH transfer fails
- MUST emit DepositFinalized and ETHBridgeFinalized events

### finalizeBridgeERC20

Completes an ERC20 bridge from L1 and mints or releases tokens to the recipient on L2.

**Parameters:**

- `_localToken`: Address of the ERC20 token on L2
- `_remoteToken`: Address of the corresponding token on L1
- `_from`: Address that initiated the bridge on L1
- `_to`: Address to receive the tokens on L2
- `_amount`: Amount of tokens being deposited
- `_extraData`: Optional data forwarded from L1 (not used for execution, only emitted)

**Behavior:**

- MUST revert if caller is not the L2CrossDomainMessenger
- MUST revert if xDomainMessageSender is not the L1StandardBridge
- MUST revert if the bridge is paused
- MUST mint tokens to `_to` if `_localToken` is an [OptimismMintableERC20](#optimismmintableerc20)
- MUST release escrowed tokens to `_to` if `_localToken` is a [Native L2 Token](#native-l2-token)
- MUST verify token pair matches for [OptimismMintableERC20](#optimismmintableerc20) tokens
- MUST decrease the deposits mapping for [Native L2 Token](#native-l2-token)s
- MUST emit DepositFinalized and ERC20BridgeFinalized events

### l1TokenBridge

Returns the address of the L1StandardBridge contract.

**Behavior:**

- MUST return the address of the otherBridge

### paused

Returns whether the bridge is paused.

**Behavior:**

- MUST always return false (L2 bridge cannot be paused)

### MESSENGER

Legacy getter for the messenger contract.

**Behavior:**

- MUST return the L2CrossDomainMessenger contract address

### OTHER_BRIDGE

Legacy getter for the other bridge contract.

**Behavior:**

- MUST return the L1StandardBridge contract address
