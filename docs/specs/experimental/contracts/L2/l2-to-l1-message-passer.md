# L2ToL1MessagePasser

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Withdrawal Transaction](#withdrawal-transaction)
  - [Withdrawal Hash](#withdrawal-hash)
  - [Message Nonce](#message-nonce)
- [Assumptions](#assumptions)
  - [a01-001: OptimismPortal validates withdrawal proofs](#a01-001-optimismportal-validates-withdrawal-proofs)
    - [Mitigations](#mitigations)
- [Invariants](#invariants)
  - [i01-001: Withdrawal commitment immutability](#i01-001-withdrawal-commitment-immutability)
    - [Impact](#impact)
  - [i01-002: Nonce monotonicity](#i01-002-nonce-monotonicity)
    - [Impact](#impact-1)
- [Function Specification](#function-specification)
  - [receive](#receive)
  - [burn](#burn)
  - [initiateWithdrawal](#initiatewithdrawal)
  - [messageNonce](#messagenonce)
  - [sentMessages](#sentmessages)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The L2ToL1MessagePasser stores commitments to withdrawal transactions initiated on L2. When users withdraw assets
from L2 to L1, they call this contract to record the withdrawal commitment. The storage root of this contract is
included in the L2 output root, allowing users to prove on L1 that a withdrawal was initiated on L2.

## Definitions

### Withdrawal Transaction

A withdrawal transaction represents an L2 to L1 message containing the sender address, target address on L1, ETH
value to transfer, gas limit for L1 execution, and arbitrary calldata. Each withdrawal is uniquely identified by a
nonce that includes a version number in its upper two bytes.

### Withdrawal Hash

The withdrawal hash is computed by keccak256 encoding the withdrawal transaction fields (nonce, sender, target,
value, gasLimit, data). This hash serves as the commitment stored in the sentMessages mapping and is used for
proving withdrawals on L1.

### Message Nonce

The message nonce is a 256-bit value where the upper 16 bits encode the message version (currently version 1) and
the lower 240 bits contain a monotonically increasing counter. This versioning scheme allows for future changes to
withdrawal transaction structure while maintaining backwards compatibility.

## Assumptions

### a01-001: OptimismPortal validates withdrawal proofs

The OptimismPortal contract on L1 properly validates withdrawal proofs against the L2ToL1MessagePasser storage root
before finalizing withdrawals. This ensures that only withdrawals with valid commitments in this contract can be
executed on L1.

#### Mitigations

- OptimismPortal uses merkle proofs to verify withdrawal commitments against the storage root
- Dispute game system validates output roots before they can be used for withdrawal finalization
- Proof maturity delay provides time for invalid output roots to be challenged

## Invariants

### i01-001: Withdrawal commitment immutability

Once a withdrawal hash is stored in the sentMessages mapping by setting it to true, it remains permanently set and
cannot be modified or removed. This ensures withdrawal commitments cannot be altered after initiation.

#### Impact

**Severity: Critical**

If withdrawal commitments could be modified or removed, attackers could prevent legitimate withdrawals from being
finalized on L1 or create fraudulent withdrawal proofs. The immutability of commitments is essential for the
security of the L2 to L1 bridge.

### i01-002: Nonce monotonicity

The message nonce strictly increases with each withdrawal initiation. The nonce counter increments by exactly one
for each call to initiateWithdrawal, ensuring each withdrawal has a unique identifier that prevents replay attacks
and maintains withdrawal ordering.

#### Impact

**Severity: High**

If nonces could be reused or skip values, multiple withdrawals could have identical hashes, allowing one proof to
finalize multiple withdrawals. Monotonic nonces ensure each withdrawal is uniquely identifiable and can only be
finalized once on L1.

## Function Specification

### receive

Allows users to withdraw ETH by sending it directly to this contract.

**Behavior:**
- MUST call initiateWithdrawal with msg.sender as target, RECEIVE_DEFAULT_GAS_LIMIT (100,000) as gas limit, and
  empty bytes as data
- MUST accept any amount of ETH sent with the transaction

### burn

Removes all ETH held by this contract from the L2 state.

**Behavior:**
- MUST retrieve the current ETH balance of the contract
- MUST burn the entire balance by creating and self-destructing a Burner contract
- MUST emit WithdrawerBalanceBurnt event with the burned amount
- MUST NOT revert regardless of the contract balance (including zero balance)

### initiateWithdrawal

Initiates a withdrawal transaction from L2 to L1 by storing a commitment.

**Parameters:**
- `_target`: Address on L1 that will receive the withdrawal call
- `_gasLimit`: Minimum gas limit that must be provided when executing the withdrawal on L1
- `_data`: Arbitrary calldata to be forwarded to the target address on L1

**Behavior:**
- MUST compute the withdrawal hash using the current message nonce, msg.sender, target parameter, msg.value,
  gasLimit parameter, and data parameter
- MUST store the withdrawal hash in sentMessages mapping by setting it to true
- MUST emit MessagePassed event with nonce, sender, target, value, gasLimit, data, and withdrawalHash
- MUST increment the internal nonce counter by one after emitting the event
- MUST accept any amount of ETH sent with the transaction (msg.value)
- MUST NOT revert under any circumstances

### messageNonce

Returns the nonce that will be used for the next withdrawal.

**Behavior:**
- MUST encode the current internal nonce counter (240 bits) with MESSAGE_VERSION (1) in the upper 16 bits
- MUST return a 256-bit value where bits 240-255 contain the version and bits 0-239 contain the nonce counter
- MUST NOT modify any state

### sentMessages

Returns whether a withdrawal commitment exists for a given hash.

**Parameters:**
- `bytes32`: The withdrawal hash to query

**Behavior:**
- MUST return true if the withdrawal hash has been stored via initiateWithdrawal
- MUST return false if the withdrawal hash has never been stored
- MUST NOT revert under any circumstances
- MUST NOT modify any state
