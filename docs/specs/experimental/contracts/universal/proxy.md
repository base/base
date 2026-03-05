# Proxy

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Admin](#admin)
  - [Implementation](#implementation)
  - [Transparent Proxy Pattern](#transparent-proxy-pattern)
- [Assumptions](#assumptions)
- [Invariants](#invariants)
  - [i01-001: Proxy transparency for non-admin users](#i01-001-proxy-transparency-for-non-admin-users)
    - [Impact](#impact)
  - [i01-002: Admin-only access to administrative interface](#i01-002-admin-only-access-to-administrative-interface)
    - [Impact](#impact-1)
- [Function Specification](#function-specification)
  - [constructor](#constructor)
  - [receive](#receive)
  - [fallback](#fallback)
  - [upgradeTo](#upgradeto)
  - [upgradeToAndCall](#upgradetoandcall)
  - [changeAdmin](#changeadmin)
  - [admin](#admin)
  - [implementation](#implementation)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The Proxy contract is a transparent proxy that enables upgradeability for smart contracts while maintaining a stable
address. It delegates all calls to an [Implementation](#implementation) contract, except when called by the
[Admin](#admin), who can access the
proxy's [administrative](#admin) interface to upgrade the [Implementation](#implementation) or transfer ownership.

## Definitions

### Admin

The address that has exclusive access to the proxy's administrative functions. The admin can upgrade the
implementation contract and transfer admin rights to a new address. The admin address is stored at the EIP-1967 admin
storage slot to prevent collisions with the implementation contract's storage.

### Implementation

The address of the contract to which all non-admin calls are delegated. The implementation contains the actual business
logic while the proxy maintains the state and provides upgradeability. The implementation address is stored at the
EIP-1967 implementation storage slot.

### Transparent Proxy Pattern

A proxy pattern where the proxy's administrative interface is only accessible to the admin address, while all other
callers have their calls transparently delegated to the implementation. This prevents function selector collisions
between the proxy's administrative functions and the implementation's functions. Additionally, calls from `address(0)`
are treated as admin calls to enable off-chain simulations via `eth_call` without requiring low-level storage
inspection.

## Assumptions

N/A

## Invariants

### i01-001: Proxy transparency for non-admin users

The proxy MUST be completely transparent to non-[Admin](#admin) users, meaning it MUST NOT affect
[Implementation](#implementation) contract
functionality in any way. All calls from non-[Admin](#admin) addresses MUST be delegated to the
[Implementation](#implementation) without
interception, modification, or validation. The proxy MUST preserve the complete execution context including
`msg.sender`, `msg.value`, calldata, and return data. The proxy's storage MUST be isolated from the implementation's
storage such that the [Implementation](#implementation) cannot access or modify the proxy's [Admin](#admin) or
[Implementation](#implementation) addresses. Non-[Admin](#admin)
callers MUST be unable to access the proxy's [administrative](#admin) interface, even if they attempt to call
[administrative](#admin)
functions, ensuring no function selector collisions between proxy and [Implementation](#implementation).

#### Impact

**Severity: Critical**

If this invariant is violated, the proxy would break the fundamental trust model of transparent proxies. Users would be
unable to interact with the [Implementation](#implementation) contract as intended, potentially losing access to funds
or functionality.
[Implementation](#implementation) contracts could compromise the proxy by modifying [Admin](#admin) or
[Implementation](#implementation) addresses. Unauthorized users
could upgrade the [Implementation](#implementation) to a malicious contract or transfer [Admin](#admin) rights, leading
to complete system
compromise.

### i01-002: Admin-only access to administrative interface

The proxy's administrative functions (`upgradeTo`, `upgradeToAndCall`, `changeAdmin`, `admin`, `implementation`) MUST
only be accessible to the admin address or `address(0)`. The `address(0)` exception enables off-chain simulations via
`eth_call` without compromising security, as `address(0)` cannot be used as `msg.sender` during normal EVM execution.

#### Impact

**Severity: Critical**

If this invariant is violated, unauthorized addresses could upgrade the [Implementation](#implementation) to a malicious
contract, steal
all funds held by the proxy, or transfer [Admin](#admin) rights, leading to complete compromise of the system.

## Function Specification

### constructor

Initializes the proxy with an [Admin](#admin) address.

**Parameters:**
- `_admin`: Address that will have administrative control over the proxy

**Behavior:**
- MUST store `_admin` at the EIP-1967 admin storage slot
- MUST emit `AdminChanged` event with `previousAdmin` as `address(0)` and `newAdmin` as `_admin`

### receive

Accepts ETH transfers and delegates to the [Implementation](#implementation).

**Behavior:**
- MUST delegate the call to the [Implementation](#implementation) contract
- MUST forward all ETH received with the call
- MUST preserve the execution context through delegatecall
- MUST return or revert based on the [Implementation](#implementation)'s response

### fallback

Handles all calls that don't match other function signatures.

**Behavior:**
- MUST delegate the call to the [Implementation](#implementation) contract
- MUST forward all calldata and ETH with the call
- MUST preserve the execution context through delegatecall
- MUST return the full returndata from the [Implementation](#implementation) if the call succeeds
- MUST revert with the full returndata from the [Implementation](#implementation) if the call fails

### upgradeTo

Updates the [Implementation](#implementation) contract address.

**Parameters:**
- `_implementation`: Address of the new implementation contract

**Behavior:**
- MUST revert if called by any address other than the admin or `address(0)`
- MUST store `_implementation` at the EIP-1967 implementation storage slot
- MUST emit `Upgraded` event with the new implementation address
- MUST delegate to [Implementation](#implementation) if called by non-[Admin](#admin) address

### upgradeToAndCall

Updates the [Implementation](#implementation) contract and executes an initialization call atomically.

**Parameters:**
- `_implementation`: Address of the new implementation contract
- `_data`: Calldata to delegatecall the new implementation with

**Behavior:**
- MUST revert if called by any address other than the admin or `address(0)`
- MUST store `_implementation` at the EIP-1967 implementation storage slot
- MUST emit `Upgraded` event with the new implementation address
- MUST perform delegatecall to `_implementation` with `_data`
- MUST revert if the delegatecall to the new [Implementation](#implementation) fails
- MUST return the returndata from the delegatecall if it succeeds
- MUST delegate to [Implementation](#implementation) if called by non-[Admin](#admin) address

### changeAdmin

Transfers [Admin](#admin) rights to a new address.

**Parameters:**
- `_admin`: Address of the new admin

**Behavior:**
- MUST revert if called by any address other than the admin or `address(0)`
- MUST store `_admin` at the EIP-1967 admin storage slot
- MUST emit `AdminChanged` event with the previous and new admin addresses
- MUST delegate to [Implementation](#implementation) if called by non-[Admin](#admin) address

### admin

Returns the current [Admin](#admin) address.

**Behavior:**
- MUST revert if called by any address other than the admin or `address(0)`
- MUST return the address stored at the EIP-1967 [Admin](#admin) storage slot
- MUST delegate to [Implementation](#implementation) if called by non-[Admin](#admin) address

### implementation

Returns the current [Implementation](#implementation) address.

**Behavior:**
- MUST revert if called by any address other than the admin or `address(0)`
- MUST return the address stored at the EIP-1967 [Implementation](#implementation) storage slot
- MUST delegate to [Implementation](#implementation) if called by non-[Admin](#admin) address
