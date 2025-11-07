# ResolvedDelegateProxy

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [AddressManager](#addressmanager)
- [Assumptions](#assumptions)
  - [a01-001: AddressManager provides valid implementation addresses](#a01-001-addressmanager-provides-valid-implementation-addresses)
    - [Mitigations](#mitigations)
  - [a01-002: Implementation storage layout compatibility](#a01-002-implementation-storage-layout-compatibility)
    - [Mitigations](#mitigations-1)
- [Invariants](#invariants)
  - [i01-001: Transparent proxy behavior](#i01-001-transparent-proxy-behavior)
    - [Impact](#impact)
  - [i01-002: Implementation address resolution requirement](#i01-002-implementation-address-resolution-requirement)
    - [Impact](#impact-1)
  - [i01-003: Immutable configuration after deployment](#i01-003-immutable-configuration-after-deployment)
    - [Impact](#impact-2)
- [Function Specification](#function-specification)
  - [constructor](#constructor)
  - [fallback](#fallback)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The ResolvedDelegateProxy is a legacy proxy contract that dynamically resolves its implementation address through the
[AddressManager](./address-manager.md) contract. It maintains backwards compatibility with pre-Bedrock versions of the
Optimism system where proxy implementations were managed through a centralized name registry rather than standard proxy
patterns.

## Definitions

### AddressManager

A legacy contract that maintains a registry mapping string names to addresses. Used in the pre-Bedrock Optimism system
for contract address resolution. The ResolvedDelegateProxy queries the AddressManager to dynamically resolve its
implementation address using a configured implementation name.

## Assumptions

### a01-001: AddressManager provides valid implementation addresses

The [AddressManager](./address-manager.md) contract returns valid, non-zero implementation addresses when queried with
the configured implementation name. If the [AddressManager](#addressmanager) returns the zero address or an invalid
address, the proxy will fail to function.

#### Mitigations

- The fallback function explicitly checks for zero address and reverts with a clear error message
- [AddressManager](#addressmanager) is controlled by governance with established processes for address updates
- Legacy contracts using this proxy pattern are being phased out in favor of standard proxy patterns

### a01-002: Implementation storage layout compatibility

The implementation contract does not use mappings in storage slots 0 or 1, which would conflict with the proxy's
`implementationName` and `addressManager` mappings.

#### Mitigations

- This storage pattern was a known design constraint in the legacy system
- Implementation contracts were designed with awareness of these reserved slots
- Modern proxy patterns (used in current OP Stack) avoid this issue entirely

## Invariants

### i01-001: Transparent proxy behavior

The proxy MUST preserve the execution context for all calls, ensuring that `msg.sender` and `msg.value` remain unchanged
when execution is delegated to the implementation contract. From the end-user's perspective, interactions with the proxy
MUST be indistinguishable from direct interactions with the implementation contract.

#### Impact

**Severity: Critical**

If this invariant is violated, the proxy would break the fundamental contract of transparent proxies. Users would lose
their identity in delegated calls, breaking authentication and authorization mechanisms in the implementation contract.
ETH transfers would fail or be misdirected, potentially leading to fund loss.

### i01-002: Implementation address resolution requirement

All calls to the proxy MUST resolve to a non-zero implementation address before delegatecall execution. The proxy MUST
revert if the [AddressManager](#addressmanager) returns the zero address for the configured implementation name.

#### Impact

**Severity: Critical**

If this invariant is violated and delegatecall proceeds to the zero address, the call would fail unpredictably. The
explicit revert ensures clear failure semantics and prevents undefined behavior.

### i01-003: Immutable configuration after deployment

The `implementationName` and `addressManager` configuration stored in the proxy MUST NOT change after deployment. These
values are set once in the constructor and have no update mechanism.

#### Impact

**Severity: High**

If this invariant were violated, the proxy could resolve to unintended implementations or query the wrong
[AddressManager](#addressmanager), breaking the contract's intended behavior. However, the contract has no functions to
modify these values, making violation impossible without storage manipulation.

## Function Specification

### constructor

Initializes the proxy with the [AddressManager](#addressmanager) reference and implementation name.

**Parameters:**
- `_addressManager`: Address of the [AddressManager](#addressmanager) contract that will resolve implementation addresses
- `_implementationName`: String name used to query the implementation address from the [AddressManager](#addressmanager)

**Behavior:**
- MUST store `_addressManager` in `addressManager[address(this)]`
- MUST store `_implementationName` in `implementationName[address(this)]`
- MUST use `address(this)` as the mapping key to avoid storage slot conflicts with implementation contracts

### fallback

Resolves the implementation address and delegates all calls to it.

**Parameters:**
- Accepts arbitrary calldata via `msg.data`
- Accepts ETH via `payable` modifier

**Behavior:**
- MUST query `addressManager[address(this)].getAddress(implementationName[address(this)])` to resolve the
  implementation address
- MUST revert if the resolved implementation address is the zero address
- MUST perform delegatecall to the resolved implementation address with `msg.data`
- MUST preserve `msg.sender` and `msg.value` through the delegatecall
- MUST return the full returndata from the delegatecall if the call succeeds
- MUST revert with the full returndata from the delegatecall if the call fails
- MUST handle returndata using inline assembly to properly forward dynamic-length data
