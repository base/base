# AddressManager

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Address Registry](#address-registry)
- [Assumptions](#assumptions)
  - [a01-001: Owner governance constraints](#a01-001-owner-governance-constraints)
    - [Mitigations](#mitigations)
- [Dependencies](#dependencies)
- [Invariants](#invariants)
  - [i01-001: Owner-exclusive write access](#i01-001-owner-exclusive-write-access)
    - [Impact](#impact)
- [Function Specification](#function-specification)
  - [setAddress](#setaddress)
  - [getAddress](#getaddress)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The AddressManager is a legacy contract that maintains a registry mapping string names to addresses. It was used in
the pre-Bedrock version of the Optimism system for contract address resolution. The system now uses a standard proxy
pattern, but this contract remains deployed for backwards compatibility with older contracts that still reference it.

## Definitions

### Address Registry

A mapping from string names to contract addresses, used in the pre-Bedrock Optimism system for contract address
resolution. Names are hashed using `keccak256(abi.encodePacked(name))` before storage lookup.

## Assumptions

### a01-001: Owner governance constraints

The owner operates within governance constraints and does not maliciously set incorrect addresses that would break
dependent legacy contracts.

#### Mitigations

- Owner is typically a multisig or governance contract with established processes
- Address changes emit events for transparency and monitoring
- Legacy contracts depending on this registry are being phased out

## Dependencies

N/A

## Invariants

### i01-001: Owner-exclusive write access

Only the contract owner can modify the name-to-address mappings via `setAddress`.

#### Impact

**Severity: High**

If this invariant were violated, unauthorized parties could modify the address registry, potentially redirecting legacy
contracts to malicious addresses. This would be Critical if assumption [a01-001] fails and the owner is compromised,
as it could lead to complete system compromise for dependent contracts.

## Function Specification

### setAddress

Changes the address associated with a string name in the registry.

**Parameters:**
- `_name`: String name to associate with an address
- `_address`: Address to map to the given name (can be zero address)

**Behavior:**
- MUST revert if caller is not the owner
- MUST compute the name hash as `keccak256(abi.encodePacked(_name))`
- MUST store the previous address before updating
- MUST set `addresses[nameHash]` to `_address`
- MUST emit `AddressSet` event with `_name`, `_address`, and the previous address

### getAddress

Retrieves the address associated with a string name.

**Parameters:**
- `_name`: String name to query

**Behavior:**
- MUST compute the name hash as `keccak256(abi.encodePacked(_name))`
- MUST return the address stored at `addresses[nameHash]`
- MUST return the zero address if no mapping exists for the given name
- MUST NOT revert under any circumstances
