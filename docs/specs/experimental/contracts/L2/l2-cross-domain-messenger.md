# L2CrossDomainMessenger

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Cross-Domain Message](#cross-domain-message)
  - [Message Replay](#message-replay)
- [Assumptions](#assumptions)
  - [a01-001: L1CrossDomainMessenger Authentication](#a01-001-l1crossdomainmessenger-authentication)
    - [Mitigations](#mitigations)
  - [a01-002: L2ToL1MessagePasser Availability](#a01-002-l2tol1messagepasser-availability)
    - [Mitigations](#mitigations-1)
- [Invariants](#invariants)
  - [i01-001: Message Replay Protection](#i01-001-message-replay-protection)
    - [Impact](#impact)
  - [i01-002: Cross-Chain Message Integrity](#i01-002-cross-chain-message-integrity)
    - [Impact](#impact-1)
  - [i01-003: Failed Message Recovery](#i01-003-failed-message-recovery)
    - [Impact](#impact-2)
  - [i01-004: Relayed Call Fidelity](#i01-004-relayed-call-fidelity)
    - [Impact](#impact-3)
  - [i01-005: L1→L2 Delivery Liveness](#i01-005-l1%E2%86%92l2-delivery-liveness)
    - [Impact](#impact-4)
- [Function Specification](#function-specification)
  - [initialize](#initialize)
  - [sendMessage](#sendmessage)
  - [relayMessage](#relaymessage)
  - [xDomainMessageSender](#xdomainmessagesender)
  - [messageNonce](#messagenonce)
  - [baseGas](#basegas)
  - [l1CrossDomainMessenger](#l1crossdomainmessenger)
  - [paused](#paused)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The L2CrossDomainMessenger provides a high-level interface for sending messages between L2 and L1. It handles
message passing in both directions, manages nonces for message ordering, provides replay protection for failed
messages, and ensures messages can only be relayed by the paired L1CrossDomainMessenger contract.

## Definitions

### Cross-Domain Message

A message sent from one domain (L1 or L2) to another, containing a target address, calldata, gas limit, and
optional ETH value. Messages are encoded with metadata including sender address and nonce for authentication and
replay protection.

### Message Replay

The ability to re-execute a failed cross-domain message. Messages that fail during initial relay (due to
insufficient gas or target revert) are marked as failed and can be replayed by anyone with sufficient gas.

## Assumptions

### a01-001: L1CrossDomainMessenger Authentication

The L1CrossDomainMessenger contract is trusted to only relay valid messages that were actually sent from L1. The
address aliasing mechanism correctly identifies messages from the L1CrossDomainMessenger.

#### Mitigations

- Address aliasing is a deterministic protocol-level mechanism
- L1CrossDomainMessenger address is set at initialization and cannot be changed

### a01-002: L2ToL1MessagePasser Availability

The L2ToL1MessagePasser predeploy contract at address 0x4200000000000000000000000000000000000016 is available and
correctly implements the initiateWithdrawal function for sending messages to L1.

#### Mitigations

- L2ToL1MessagePasser is a protocol predeploy at a fixed address
- Contract is part of the core L2 system and cannot be modified

## Invariants

### i01-001: Message Replay Protection

Successfully relayed messages cannot be replayed. Each [message hash](../L1/l1-cross-domain-messenger.md#message-hash)
can only be marked as successful once, and attempting to relay an already-successful message will revert.

#### Impact

**Severity: Critical**

Without replay protection, an attacker could replay messages multiple times, potentially draining funds or causing
unauthorized state changes on the target contract.

### i01-002: Cross-Chain Message Integrity

Messages can only be relayed if they originate from the paired L1CrossDomainMessenger or are being replayed after
an initial failure. Unauthorized parties cannot inject or forge cross-domain messages.

#### Impact

**Severity: Critical**

If message authentication fails, attackers could execute arbitrary calls on L2 with spoofed sender addresses,
completely compromising the security of cross-chain applications.

### i01-003: Failed Message Recovery

Messages that fail during relay (due to insufficient gas or target revert) are marked as failed and can be
replayed by any party. Failed messages cannot be marked as successful, and successful messages cannot be marked as
failed.

#### Impact

**Severity: High**

Without proper failed message handling, users could lose funds or have messages permanently stuck if initial relay
attempts fail. The replay mechanism ensures messages can eventually be executed with sufficient gas.

### i01-004: Relayed Call Fidelity

Any relayed message executes a call whose target, calldata bytes, and ETH value are exactly those committed by the
initiating message, and during that call the xDomainMessageSender observed by the target equals the committed
sender. This holds for the initial relay and any replay.

#### Impact

**Severity: Critical**

Tampering with calldata, value, or sender would enable arbitrary execution or fund loss.

### i01-005: L1→L2 Delivery Liveness

All valid sent messages from L1 to L2 are received within a bounded amount of time less than 1 day. This time
bound is provided by the derivation pipeline and sequencing window, while the messenger processes messages as
they arrive.

#### Impact

**Severity: High**

Without timely message delivery, users could experience indefinite delays in cross-chain operations, potentially
leading to fund loss, failed time-sensitive transactions, or denial of service for critical bridge functionality.

## Function Specification

### initialize

Initializes the L2CrossDomainMessenger with the address of the paired L1CrossDomainMessenger contract.

**Parameters:**

- `_l1CrossDomainMessenger`: Address of the L1CrossDomainMessenger contract on L1

**Behavior:**

- MUST only be callable once due to initializer modifier
- MUST set the otherMessenger to the provided L1CrossDomainMessenger address
- MUST initialize xDomainMsgSender to the default value if not already set
- MUST disable further initialization attempts

### sendMessage

Sends a message from L2 to L1 by encoding it and passing it to the L2ToL1MessagePasser predeploy.

**Parameters:**

- `_target`: Target contract or wallet address on L1
- `_message`: Calldata to send to the target address
- `_minGasLimit`: Minimum gas limit for executing the message on L1

**Behavior:**

- MUST accept ETH value via msg.value to be sent with the message
- MUST compute the full gas limit using baseGas function
- MUST encode the message with relayMessage selector, current nonce, sender, target, value, minGasLimit, and
  message data
- MUST call L2ToL1MessagePasser.initiateWithdrawal with the encoded message and ETH value
- MUST emit SentMessage event with target, sender, message, nonce, and minGasLimit
- MUST emit SentMessageExtension1 event with sender and value
- MUST increment the [message nonce](../L1/l1-cross-domain-messenger.md#message-nonce) after sending

### relayMessage

Relays a message that was sent from L1 to L2. Can be called by the L1CrossDomainMessenger (via deposit transaction)
or by anyone replaying a previously failed message.

**Parameters:**

- `_nonce`: Nonce of the message being relayed
- `_sender`: Address of the user who sent the message on L1
- `_target`: Target address on L2
- `_value`: ETH value to send with the message
- `_minGasLimit`: Minimum gas limit for executing the message
- `_message`: Calldata to send to the target

**Behavior:**

- MUST revert if the contract is paused (always returns false on L2)
- MUST revert if message version is greater than 1
- MUST check legacy version 0 message hash is not already relayed for migrated withdrawals
- MUST compute version 1 message hash from all parameters
- MUST verify msg.value equals _value and message is not already failed when called by L1CrossDomainMessenger
- MUST verify msg.value is zero and message is marked as failed when called by anyone else (replay)
- MUST revert if target is an unsafe system address (self or L2ToL1MessagePasser)
- MUST revert if message hash is already marked as successful
- MUST mark message as failed if insufficient gas remains or reentrancy is detected
- MUST set xDomainMsgSender to _sender before external call
- MUST execute external call to target with remaining gas minus reserved gas
- MUST reset xDomainMsgSender to default value after external call
- MUST mark message as successful and emit RelayedMessage event if call succeeds
- MUST mark message as failed and emit FailedRelayedMessage event if call fails
- MUST revert if called from estimation address and message fails

### xDomainMessageSender

Returns the address of the sender of the currently executing cross-domain message.

**Behavior:**

- MUST revert if no message is currently being executed (xDomainMsgSender is default value)
- MUST return the sender address of the currently executing message

### messageNonce

Returns the nonce that will be used for the next sent message, with the message version encoded in the upper 2
bytes.

**Behavior:**

- MUST return the current msgNonce with MESSAGE_VERSION (1) encoded in the upper 2 bytes
- MUST be a view function with no state changes

### baseGas

Computes the minimum gas required to guarantee a message will be received on L1 without running out of gas.

**Parameters:**

- `_message`: Message calldata
- `_minGasLimit`: Minimum desired gas limit for the target call

**Behavior:**

- MUST compute execution gas including relay overhead, call overhead, reserved gas, gas check buffer, and adjusted
  minGasLimit accounting for EIP-150
- MUST compute total message size including encoding overhead
- MUST return the maximum of execution gas plus calldata gas cost and the EIP-7623 calldata floor cost
- MUST be a pure function with no state access

### l1CrossDomainMessenger

Legacy getter for the paired L1CrossDomainMessenger address.

**Behavior:**

- MUST return the otherMessenger address
- MUST be a view function with no state changes

### paused

Returns whether the contract is paused. Always returns false on L2.

**Behavior:**

- MUST return false
- MUST be a view function with no state changes
