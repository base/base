# LivenessGuard

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Liveness Timestamp](#liveness-timestamp)
- [Assumptions](#assumptions)
- [Invariants](#invariants)
  - [i01-001: Liveness tracking accuracy](#i01-001-liveness-tracking-accuracy)
    - [Impact](#impact)
  - [i02-002: Liveness demonstration capability](#i02-002-liveness-demonstration-capability)
    - [Impact](#impact-1)
- [Function Specification](#function-specification)
  - [constructor](#constructor)
  - [safe](#safe)
  - [checkTransaction](#checktransaction)
  - [checkAfterExecution](#checkafterexecution)
  - [showLiveness](#showliveness)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The LivenessGuard tracks the liveness of Safe multisig owners by recording timestamps when owners sign transactions or
explicitly demonstrate activity. It serves as a monitoring mechanism to identify inactive owners who may have lost
access to their keys.

## Definitions

### Liveness Timestamp

The most recent block timestamp at which an owner either signed a Safe transaction or called the `showLiveness`
function. This timestamp is publicly queryable via the `lastLive` mapping.

## Assumptions

N/A

## Invariants

### i01-001: Liveness tracking accuracy

Liveness for each owner is tracked accurately. The `lastLive` mapping correctly reflects when owners sign transactions
or call `showLiveness`, and owner additions/removals are properly synchronized with the Safe's owner set.

#### Impact

**Severity: High**

If liveness tracking is inaccurate, inactive owners could appear active (preventing their removal) or active owners
could appear inactive (enabling improper removal). This undermines the entire liveness monitoring mechanism and could
lead to unauthorized changes to the Safe's owner set.

### i02-002: Liveness demonstration capability

Owners are always capable of showing liveness if the owner is actually live. The guard never prevents legitimate owners
from demonstrating their liveness through either transaction signatures or direct `showLiveness` calls.

#### Impact

**Severity: Critical**

If legitimate owners cannot demonstrate liveness, they could be incorrectly identified as inactive and removed from the
Safe. This could lead to loss of access to the Safe's assets and functionality, potentially causing permanent fund
loss if enough owners are incorrectly removed.

## Function Specification

### constructor

Initializes the LivenessGuard for a specific Safe contract and records all current owners with the deployment
timestamp.

**Parameters:**

- `_safe`: The Safe contract instance that this guard will monitor

**Behavior:**

- MUST set the immutable `SAFE` reference to the provided Safe contract
- MUST query the Safe's current owner list via `getOwners()`
- MUST set `lastLive[owner]` to `block.timestamp` for each current owner
- MUST emit `OwnerRecorded` event for each current owner

### safe

Returns the Safe contract instance that this guard monitors.

**Behavior:**

- MUST return the immutable `SAFE` contract reference

### checkTransaction

Records liveness for owners who signed the current transaction. Called by the Safe before transaction execution.

**Parameters:**

- `_to`: Transaction target address
- `_value`: ETH value to send
- `_data`: Transaction calldata
- `_operation`: Operation type (Call or DelegateCall)
- `_safeTxGas`: Gas allocated for Safe transaction execution
- `_baseGas`: Gas costs independent of transaction execution
- `_gasPrice`: Gas price for refund calculation
- `_gasToken`: Token address for gas payment (zero address for ETH)
- `_refundReceiver`: Address receiving gas refund
- `_signatures`: Packed signature data from Safe owners
- `_msgSender`: Original transaction sender

**Behavior:**

- MUST revert if caller is not the associated Safe contract
- MUST cache the current Safe owner list for comparison after transaction execution
- MUST reconstruct the transaction hash using Safe's `getTransactionHash()` with nonce decremented by 1
- MUST extract exactly `threshold` number of signers from `_signatures` using `SafeSigners.getNSigners()`
- MUST set `lastLive[signer]` to `block.timestamp` for each extracted signer
- MUST emit `OwnerRecorded` event for each signer
- MUST NOT revert due to signature parsing or owner extraction failures

### checkAfterExecution

Updates the `lastLive` mapping to reflect any changes in the Safe's owner set. Called by the Safe after transaction
execution.

**Parameters:**

- First parameter (unnamed): Transaction hash (unused)
- Second parameter (unnamed): Transaction success status (unused)

**Behavior:**

- MUST revert if caller is not the associated Safe contract
- MUST query the current Safe owner list via `getOwners()`
- MUST process each current owner:
  - If owner existed before transaction execution, mark as processed
  - If owner did not exist before transaction execution, set `lastLive[owner]` to `block.timestamp` (new owner added)
- MUST process each owner that existed before but not after transaction execution:
  - Delete `lastLive[owner]` entry (owner was removed from Safe)
- MUST ensure internal owner tracking state is completely cleared after execution
- MUST NOT revert due to owner set changes or state inconsistencies

### showLiveness

Allows a Safe owner to directly demonstrate liveness without signing a transaction.

**Behavior:**

- MUST revert if caller is not a current owner of the Safe (verified via `SAFE.isOwner()`)
- MUST set `lastLive[msg.sender]` to `block.timestamp`
- MUST emit `OwnerRecorded` event with `msg.sender` as the owner
