# LegacyMessagePasser

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
- [Assumptions](#assumptions)
  - [a01-001: Migration completeness](#a01-001-migration-completeness)
    - [Mitigations](#mitigations)
  - [a01-002: Alternative bridge compatibility](#a01-002-alternative-bridge-compatibility)
    - [Mitigations](#mitigations-1)
- [Dependencies](#dependencies)
- [Invariants](#invariants)
  - [i01-001: Message commitment immutability](#i01-001-message-commitment-immutability)
    - [Impact](#impact)
  - [i01-002: Unrestricted message passing](#i01-002-unrestricted-message-passing)
    - [Impact](#impact-1)
- [Function Specification](#function-specification)
  - [passMessageToL1](#passmessagetol1)
  - [sentMessages](#sentmessages)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The LegacyMessagePasser is a deprecated predeploy contract that stored commitments to withdrawal transactions
before the Bedrock upgrade. It has been superseded by the L2ToL1MessagePasser contract and is no longer used for
withdrawals through the CrossDomainMessenger system.

## Definitions

N/A

## Assumptions

### a01-001: Migration completeness

All pending withdrawals from this contract were successfully migrated to the L2ToL1MessagePasser during the
Bedrock upgrade.

#### Mitigations

- Migration was performed as part of the Bedrock upgrade process
- Historical withdrawal data remains accessible for verification

### a01-002: Alternative bridge compatibility

Alternative bridge implementations that depend on this contract have either migrated to the new system or are aware
that finalization through this contract is no longer supported.

#### Mitigations

- Clear deprecation notices in documentation
- Contract remains deployed to maintain historical state

## Dependencies

This specification depends on:
- [L2ToL1MessagePasser](../../protocol/predeploys.md#l2tol1messagepasser) - The successor contract that replaced this functionality

## Invariants

### i01-001: Message commitment immutability

Once a message commitment is stored in the `sentMessages` mapping, it remains permanently set to `true` and cannot
be modified or removed.

#### Impact

**Severity: Low**

If this invariant were violated, historical withdrawal commitments could be altered, potentially affecting the
verification of pre-Bedrock withdrawals. However, since the contract is deprecated and no longer used for
finalization, the practical impact is minimal and limited to historical record integrity.

### i01-002: Unrestricted message passing

Any address can call `passMessageToL1` to store a message commitment without restrictions or validation.

#### Impact

**Severity: Low**

If this invariant were violated and restrictions were added, it would deviate from the contract's original design.
However, since the contract is deprecated and calling it is a no-op in the context of the CrossDomainMessenger
system, this has no practical security impact.

## Function Specification

### passMessageToL1

Stores a commitment to a message in the `sentMessages` mapping.

**Parameters:**
- `_message`: Arbitrary bytes representing the message to pass to L1

**Behavior:**
- MUST compute the message hash as `keccak256(abi.encodePacked(_message, msg.sender))`
- MUST set `sentMessages[messageHash]` to `true`
- MUST NOT revert under any circumstances
- MUST NOT emit any events
- MUST NOT forward the call to L2ToL1MessagePasser

### sentMessages

Returns whether a message commitment exists for a given hash.

**Parameters:**
- `bytes32`: The message hash to query

**Behavior:**
- MUST return `true` if the message hash has been stored via `passMessageToL1`
- MUST return `false` if the message hash has never been stored
- MUST NOT revert under any circumstances
