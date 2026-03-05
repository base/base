# L1ChugSplashProxy

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
- [Assumptions](#assumptions)
  - [a01-001: Owner operates within governance constraints](#a01-001-owner-operates-within-governance-constraints)
    - [Mitigations](#mitigations)
- [Invariants](#invariants)
  - [i01-001: Transparent Delegation](#i01-001-transparent-delegation)
    - [Impact](#impact)
  - [i01-002: Owner-Only Administrative Control](#i01-002-owner-only-administrative-control)
    - [Impact](#impact-1)
- [Function Specification](#function-specification)
  - [constructor](#constructor)
  - [receive](#receive)
  - [fallback](#fallback)
  - [setCode](#setcode)
  - [setStorage](#setstorage)
  - [setOwner](#setowner)
  - [getOwner](#getowner)
  - [getImplementation](#getimplementation)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

Legacy proxy contract enabling transparent delegation to implementation contracts with additional capabilities for
direct code and storage manipulation during upgrades.

## Definitions

N/A

## Assumptions

### a01-001: Owner operates within governance constraints

The owner address is expected to be controlled by governance processes and will not maliciously exploit direct storage
manipulation capabilities to corrupt contract state.

#### Mitigations

- Owner is typically a multisig or governance contract
- Upgrades follow established governance procedures
- Community monitoring of owner actions

## Invariants

### i01-001: Transparent Delegation

All calls from non-owner addresses are delegated to the implementation contract without interception, preserving the
implementation's complete functionality and state context.

#### Impact

**Severity: Critical**

Violation would break the proxy's core purpose, potentially preventing users from accessing implementation functions or
causing unexpected behavior where proxy logic interferes with delegated calls.

### i01-002: Owner-Only Administrative Control

Only the owner can execute administrative operations including code updates, storage modifications, ownership transfers,
and querying administrative state.

#### Impact

**Severity: Critical**

Violation would allow unauthorized parties to deploy arbitrary code via `setCode`, manipulate storage via `setStorage`,
or transfer ownership, enabling complete takeover of the proxy and any assets it controls.

## Function Specification

### constructor

Initializes the proxy with an owner address.

**Parameters:**

- `_owner`: Address that will have administrative control over the proxy

**Behavior:**

- MUST set the owner to `_owner` in the EIP-1967 admin storage slot

### receive

Handles plain ETH transfers to the proxy.

**Behavior:**

- MUST delegate the call to the implementation contract
- MUST revert if no implementation is set
- MUST revert if the owner's `isUpgrading()` returns true

### fallback

Handles all function calls not matching other function signatures.

**Behavior:**

- MUST delegate the call to the implementation contract
- MUST revert if no implementation is set
- MUST revert if the owner's `isUpgrading()` returns true

### setCode

Deploys new bytecode as the implementation contract.

**Parameters:**

- `_code`: Bytecode to deploy as the new implementation

**Behavior:**

- MUST revert if caller is not the owner and not address(0)
- MUST delegate to implementation if caller is not the owner and not address(0)
- MUST return early without changes if the code hash matches the current implementation's code hash
- MUST prepend the deployment prefix `0x600D380380600D6000396000f3` to the provided bytecode
- MUST deploy the prefixed bytecode using CREATE
- MUST revert if the deployed code hash does not match the provided code hash
- MUST update the implementation address in the EIP-1967 implementation storage slot

### setStorage

Directly modifies a storage slot in the proxy contract.

**Parameters:**

- `_key`: Storage slot to modify
- `_value`: New value for the storage slot

**Behavior:**

- MUST revert if caller is not the owner and not address(0)
- MUST delegate to implementation if caller is not the owner and not address(0)
- MUST write `_value` to storage slot `_key`

### setOwner

Transfers ownership of the proxy to a new address.

**Parameters:**

- `_owner`: Address of the new owner

**Behavior:**

- MUST revert if caller is not the owner and not address(0)
- MUST delegate to implementation if caller is not the owner and not address(0)
- MUST update the owner address in the EIP-1967 admin storage slot

### getOwner

Retrieves the current owner address.

**Behavior:**

- MUST revert if caller is not the owner and not address(0)
- MUST delegate to implementation if caller is not the owner and not address(0)
- MUST return the owner address from the EIP-1967 admin storage slot

### getImplementation

Retrieves the current implementation address.

**Behavior:**

- MUST revert if caller is not the owner and not address(0)
- MUST delegate to implementation if caller is not the owner and not address(0)
- MUST return the implementation address from the EIP-1967 implementation storage slot
