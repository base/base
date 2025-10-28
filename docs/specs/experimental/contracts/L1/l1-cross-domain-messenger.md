# L1CrossDomainMessenger

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Message Hash](#message-hash)
  - [Message Nonce](#message-nonce)
  - [Base Gas](#base-gas)
- [Assumptions](#assumptions)
  - [a01-001: OptimismPortal Relays Valid Messages](#a01-001-optimismportal-relays-valid-messages)
    - [Mitigations](#mitigations)
  - [a01-002: SystemConfig Provides Accurate Pause State](#a01-002-systemconfig-provides-accurate-pause-state)
    - [Mitigations](#mitigations-1)
- [Invariants](#invariants)
  - [i01-001: All Valid Messages Can Be Executed Within Bounded Time](#i01-001-all-valid-messages-can-be-executed-within-bounded-time)
    - [Impact](#impact)
  - [i01-002: Valid Messages Are Executed Faithfully](#i01-002-valid-messages-are-executed-faithfully)
    - [Impact](#impact-1)
  - [i01-003: Invalid Messages Cannot Be Executed](#i01-003-invalid-messages-cannot-be-executed)
    - [Impact](#impact-2)
  - [i01-004: Messages Can Only Be Executed Once](#i01-004-messages-can-only-be-executed-once)
    - [Impact](#impact-3)
  - [i01-005: Messages Can Always Be Sent To The Other Side By Any User](#i01-005-messages-can-always-be-sent-to-the-other-side-by-any-user)
    - [Impact](#impact-4)
- [Function Specification](#function-specification)
  - [initialize](#initialize)
  - [sendMessage](#sendmessage)
  - [relayMessage](#relaymessage)
  - [xDomainMessageSender](#xdomainmessagesender)
  - [paused](#paused)
  - [superchainConfig](#superchainconfig)
  - [PORTAL](#portal)
  - [messageNonce](#messagenonce)
  - [baseGas](#basegas)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The L1CrossDomainMessenger provides a high-level interface for sending messages between L1 and L2. It enables
applications to communicate across domains without directly interacting with lower-level system contracts. The contract
handles message replay for failed messages and integrates with the OptimismPortal for cross-chain message delivery.

## Definitions

### Message Hash

A unique identifier for a cross-domain message computed from the message parameters including nonce, sender, target,
value, gas limit, and message data. Version 1 messages use a hash that commits to all parameters.

### Message Nonce

A monotonically increasing counter that uniquely identifies each sent message. The upper two bytes encode the message
version while the lower bytes contain the sequential nonce value.

### Base Gas

The total gas required to guarantee a message will be received on the other chain without running out of gas. This
includes execution gas, calldata costs, and overhead for the relay mechanism.

## Assumptions

### a01-001: OptimismPortal Relays Valid Messages

The OptimismPortal contract correctly relays messages from L2 by setting the l2Sender value and only calling
relayMessage for messages that have been proven and finalized through the withdrawal process.

#### Mitigations

- OptimismPortal enforces proof verification and finalization delays before allowing message relay
- Messages can only be relayed through the portal, not directly

### a01-002: SystemConfig Provides Accurate Pause State

The SystemConfig contract accurately reflects the pause state from the SuperchainConfig, allowing the messenger to
respect system-wide pause functionality.

#### Mitigations

- Pause state is controlled by governance through the SuperchainConfig guardian role
- Pause functionality is a safety mechanism for emergency situations

## Invariants

### i01-001: All Valid Messages Can Be Executed Within Bounded Time

All valid messages sent from L2 to L1 can eventually be relayed and executed on L1 after the proof and finalization
periods, provided the message does not target blocked addresses and has sufficient gas.

#### Impact

**Severity: High**

This liveness property ensures that legitimate cross-domain communication cannot be permanently blocked. Violation
would allow censorship of cross-domain messages or permanent locking of assets in bridge contracts.

### i01-002: Valid Messages Are Executed Faithfully

Messages relayed on L1 execute with the exact sender, target, value, and data that were specified when the message was
sent from L2. No parameter can be modified during transmission.

#### Impact

**Severity: Critical**

If this invariant is violated, attackers could modify message parameters during transmission, leading to unauthorized
fund transfers, incorrect state changes, or impersonation of legitimate senders.

### i01-003: Invalid Messages Cannot Be Executed

Messages that do not originate from the L2CrossDomainMessenger through the OptimismPortal, or messages targeting
blocked system addresses, cannot be executed.

#### Impact

**Severity: Critical**

If this invariant is violated, attackers could forge messages that appear to come from L2, bypassing the security
guarantees of the bridge and potentially stealing funds or compromising system integrity.

### i01-004: Messages Can Only Be Executed Once

Once a message has been successfully relayed and marked in the successfulMessages mapping, it cannot be relayed again
regardless of who attempts to relay it.

#### Impact

**Severity: Critical**

If this invariant is violated, an attacker could replay successful messages multiple times, potentially draining funds
or causing unauthorized state changes on the target contract.

### i01-005: Messages Can Always Be Sent To The Other Side By Any User

Any user can call sendMessage to send a message to L2 at any time when the system is not paused. There are no
restrictions on who can initiate cross-domain messages.

#### Impact

**Severity: High**

This liveness property ensures that the messaging system remains permissionless and censorship-resistant. Violation
would allow selective blocking of users from sending cross-domain messages.

## Function Specification

### initialize

Initializes the L1CrossDomainMessenger with references to the SystemConfig and OptimismPortal contracts.

**Parameters:**
- `_systemConfig`: The SystemConfig contract address for this network
- `_portal`: The OptimismPortal contract address for this network

**Behavior:**
- MUST revert if caller is not the ProxyAdmin or ProxyAdmin owner
- MUST revert if contract has already been initialized to the current version
- MUST set the systemConfig storage variable to the provided address
- MUST set the portal storage variable to the provided address
- MUST set otherMessenger to the L2CrossDomainMessenger predeploy address
- MUST set xDomainMsgSender to the default value if it is currently zero
- MUST emit an Initialized event

### sendMessage

Sends a message to a target address on L2 by depositing a transaction through the OptimismPortal.

**Parameters:**
- `_target`: Target contract or wallet address on L2
- `_message`: Calldata to send to the target
- `_minGasLimit`: Minimum gas limit for executing the message on L2

**Behavior:**
- MUST calculate base gas using the baseGas function with the message and minimum gas limit
- MUST encode a call to relayMessage with the current nonce, sender, target, value, gas limit, and message
- MUST call portal.depositTransaction with the encoded message, base gas, and msg.value
- MUST emit SentMessage event with target, sender, message, nonce, and gas limit
- MUST emit SentMessageExtension1 event with sender and value
- MUST increment the message nonce by one
- MUST accept any value of ETH sent with the call and forward it to the portal

### relayMessage

Relays a message that was sent from L2 through the OptimismPortal or replays a previously failed message.

**Parameters:**
- `_nonce`: Versioned nonce of the message
- `_sender`: Address that sent the message on L2
- `_target`: Target address for the message on L1
- `_value`: ETH value to send with the message
- `_minGasLimit`: Minimum gas limit for executing the message
- `_message`: Message data to send to the target

**Behavior:**
- MUST revert if the contract is paused
- MUST revert if message version is not 0 or 1
- MUST revert if version 0 message has already been successfully relayed using legacy hash
- MUST compute version 1 message hash from all parameters
- MUST revert if caller is not the portal and msg.value is not zero
- MUST revert if caller is not the portal and message is not in failedMessages mapping
- MUST revert if target is the messenger itself or the portal
- MUST revert if message has already been successfully relayed
- MUST mark message as failed and emit FailedRelayedMessage if insufficient gas remains
- MUST mark message as failed and emit FailedRelayedMessage if xDomainMsgSender is not the default value
- MUST set xDomainMsgSender to the sender address before calling target
- MUST call target with remaining gas minus reserved gas, the value, and the message data
- MUST reset xDomainMsgSender to default value after the call completes
- MUST mark message as successful and emit RelayedMessage if call succeeds
- MUST mark message as failed and emit FailedRelayedMessage if call fails
- MUST revert if call fails and transaction origin is the estimation address
- MUST accept ETH value equal to the message value parameter when called by the portal

### xDomainMessageSender

Returns the address of the sender of the currently executing cross-domain message.

**Behavior:**
- MUST revert if no message is currently being executed
- MUST return the sender address of the active message when called during message execution

### paused

Returns whether the messenger is currently paused.

**Behavior:**
- MUST return the pause state from the SystemConfig contract
- MUST query systemConfig.paused() for the current pause status

### superchainConfig

Returns the SuperchainConfig contract address.

**Behavior:**
- MUST return the SuperchainConfig address from the SystemConfig contract
- MUST query systemConfig.superchainConfig() for the address

### PORTAL

Legacy getter for the OptimismPortal contract address.

**Behavior:**
- MUST return the same address as the portal() function
- MUST return the portal storage variable value

### messageNonce

Returns the nonce for the next message to be sent with the message version encoded in the upper two bytes.

**Behavior:**
- MUST encode the current msgNonce with MESSAGE_VERSION in the upper two bytes
- MUST return a uint256 with version in bits 240-255 and nonce in bits 0-239

### baseGas

Computes the total gas required to guarantee message delivery on the other chain.

**Parameters:**
- `_message`: Message data to compute gas for
- `_minGasLimit`: Minimum desired gas limit for message execution

**Behavior:**
- MUST calculate execution gas including relay overhead, call overhead, reserved gas, and gas check buffer
- MUST account for EIP-150 gas forwarding by multiplying minimum gas limit by 64/63
- MUST calculate total message size including encoding overhead
- MUST return the maximum of execution gas plus calldata costs or the EIP-7623 floor cost
- MUST ensure returned value covers transaction base gas plus all overhead
