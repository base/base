# ProxyAdmin

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [ERC1967 Proxy](#erc1967-proxy)
  - [ChugSplash Proxy](#chugsplash-proxy)
  - [ResolvedDelegate Proxy](#resolveddelegate-proxy)
- [Assumptions](#assumptions)
- [Invariants](#invariants)
  - [i01-001: Owner-Only Administrative Control](#i01-001-owner-only-administrative-control)
    - [Impact](#impact)
  - [i01-002: Multi-Proxy Type Administration](#i01-002-multi-proxy-type-administration)
    - [Impact](#impact-1)
- [Function Specification](#function-specification)
  - [constructor](#constructor)
  - [setProxyType](#setproxytype)
  - [setImplementationName](#setimplementationname)
  - [setAddressManager](#setaddressmanager)
  - [setAddress](#setaddress)
  - [setUpgrading](#setupgrading)
  - [isUpgrading](#isupgrading)
  - [getProxyImplementation](#getproxyimplementation)
  - [getProxyAdmin](#getproxyadmin)
  - [changeProxyAdmin](#changeproxyadmin)
  - [upgrade](#upgrade)
  - [upgradeAndCall](#upgradeandcall)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

Manages administrative operations for multiple proxy types, including [ERC1967 Proxies](#erc1967-proxy), legacy
[ChugSplash Proxies](#chugsplash-proxy), and
[ResolvedDelegate Proxies](#resolveddelegate-proxy).

## Definitions

### ERC1967 Proxy

Standard transparent proxy following EIP-1967 specification with separate admin and implementation storage slots.

### ChugSplash Proxy

Legacy proxy type used in earlier Optimism deployments with custom storage layout and upgrade mechanisms.

### ResolvedDelegate Proxy

Legacy proxy type that resolves implementation addresses through an AddressManager contract using string-based name
lookups.

## Assumptions

N/A

## Invariants

### i01-001: Owner-Only Administrative Control

Only the contract owner can execute administrative operations including proxy upgrades, admin changes, proxy type
configuration, and AddressManager modifications.

#### Impact

**Severity: Critical**

Violation would allow unauthorized parties to upgrade proxy implementations or change proxy administrators, enabling
complete takeover of managed contracts and potential theft of all assets controlled by those proxies.

### i01-002: Multi-Proxy Type Administration

The contract can successfully administrate all three proxy types (ERC1967, ChugSplash, and ResolvedDelegate) through
their respective type-specific interfaces and storage layouts.

#### Impact

**Severity: High**

Failure to support any proxy type would prevent administration of legacy proxies during migrations, potentially
leaving critical infrastructure without upgrade capabilities or administrative control.

## Function Specification

### constructor

Initializes the ProxyAdmin with an owner address.

**Parameters:**

- `_owner`: Address that will have administrative control over the ProxyAdmin

**Behavior:**

- MUST transfer ownership to `_owner`

### setProxyType

Configures the proxy type for a given proxy address.

**Parameters:**

- `_address`: Address of the proxy to configure
- `_type`: ProxyType enum value (ERC1967, CHUGSPLASH, or RESOLVED)

**Behavior:**

- MUST revert if caller is not the owner
- MUST store the proxy type in the `proxyType` mapping

### setImplementationName

Sets the implementation name for a [ResolvedDelegate Proxy](#resolveddelegate-proxy).

**Parameters:**

- `_address`: Address of the ResolvedDelegate proxy
- `_name`: String identifier used to resolve the implementation address via AddressManager

**Behavior:**

- MUST revert if caller is not the owner
- MUST store the implementation name in the `implementationName` mapping

### setAddressManager

Configures the AddressManager contract used for [ResolvedDelegate Proxy](#resolveddelegate-proxy) resolution.

**Parameters:**

- `_address`: Address of the AddressManager contract

**Behavior:**

- MUST revert if caller is not the owner
- MUST store the AddressManager address

### setAddress

Sets an address in the AddressManager contract.

**Parameters:**

- `_name`: String identifier for the address
- `_address`: Address to associate with the name

**Behavior:**

- MUST revert if caller is not the owner
- MUST call `setAddress` on the AddressManager contract

### setUpgrading

Controls the upgrading status flag for [ChugSplash Proxies](#chugsplash-proxy).

**Parameters:**

- `_upgrading`: Boolean indicating whether an upgrade is in progress

**Behavior:**

- MUST revert if caller is not the owner
- MUST set the internal `upgrading` state variable

### isUpgrading

Returns the current upgrading status.

**Behavior:**

- MUST return the value of the `upgrading` state variable

### getProxyImplementation

Retrieves the implementation address for a given proxy.

**Parameters:**

- `_proxy`: Address of the proxy

**Behavior:**

- MUST return the implementation address by calling the appropriate interface method based on the proxy's configured
  type
- For ERC1967 proxies: MUST call `implementation()` on the proxy
- For CHUGSPLASH proxies: MUST call `getImplementation()` on the proxy
- For RESOLVED proxies: MUST query AddressManager using the stored implementation name
- MUST revert if the proxy type is not configured

### getProxyAdmin

Retrieves the admin address for a given proxy.

**Parameters:**

- `_proxy`: Address of the proxy

**Behavior:**

- MUST return the admin address by calling the appropriate interface method based on the proxy's configured type
- For ERC1967 proxies: MUST call `admin()` on the proxy
- For CHUGSPLASH proxies: MUST call `getOwner()` on the proxy
- For RESOLVED proxies: MUST return the owner of the AddressManager
- MUST revert if the proxy type is not configured

### changeProxyAdmin

Updates the admin address for a given proxy.

**Parameters:**

- `_proxy`: Address of the proxy to update
- `_newAdmin`: Address of the new admin

**Behavior:**

- MUST revert if caller is not the owner
- For ERC1967 proxies: MUST call `changeAdmin(_newAdmin)` on the proxy
- For CHUGSPLASH proxies: MUST call `setOwner(_newAdmin)` on the proxy
- For RESOLVED proxies: MUST call `transferOwnership(_newAdmin)` on the AddressManager
- MUST revert if the proxy type is not configured

### upgrade

Changes a proxy's implementation contract.

**Parameters:**

- `_proxy`: Address of the proxy to upgrade
- `_implementation`: Address of the new implementation

**Behavior:**

- MUST revert if caller is not the owner
- For ERC1967 proxies: MUST call `upgradeTo(_implementation)` on the proxy
- For CHUGSPLASH proxies: MUST write `_implementation` to the PROXY_IMPLEMENTATION_ADDRESS storage slot
- For RESOLVED proxies: MUST call `setAddress` on AddressManager with the stored implementation name and new
  implementation address
- MUST revert via assertion if proxy type is not one of the three defined types

### upgradeAndCall

Changes a proxy's implementation and executes an initialization call.

**Parameters:**

- `_proxy`: Address of the proxy to upgrade
- `_implementation`: Address of the new implementation
- `_data`: Calldata to execute on the proxy after upgrade

**Behavior:**

- MUST revert if caller is not the owner
- For ERC1967 proxies: MUST call `upgradeToAndCall(_implementation, _data)` with forwarded msg.value
- For CHUGSPLASH and RESOLVED proxies: MUST call `upgrade(_proxy, _implementation)` then execute a call to `_proxy`
  with `_data` and forwarded msg.value
- MUST revert if the post-upgrade call fails
- MUST revert if the proxy type is not configured
