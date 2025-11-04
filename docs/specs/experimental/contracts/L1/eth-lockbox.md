# ETHLockbox

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Definitions](#definitions)
  - [Authorized Portal](#authorized-portal)
  - [Authorized Lockbox](#authorized-lockbox)
  - [Liquidity Migration](#liquidity-migration)
- [Assumptions](#assumptions)
  - [a01-001: ProxyAdmin Owner Trusted](#a01-001-proxyadmin-owner-trusted)
    - [Mitigations](#mitigations)
  - [a01-002: Authorized Portals Trusted](#a01-002-authorized-portals-trusted)
    - [Mitigations](#mitigations-1)
- [Invariants](#invariants)
  - [i01-001: Portal Authorization Required](#i01-001-portal-authorization-required)
    - [Impact](#impact)
  - [i01-002: Always Accessible](#i01-002-always-accessible)
    - [Impact](#impact-1)
- [Function Specification](#function-specification)
  - [initialize](#initialize)
  - [lockETH](#locketh)
  - [unlockETH](#unlocketh)
  - [authorizePortal](#authorizeportal)
  - [authorizeLockbox](#authorizelockbox)
  - [migrateLiquidity](#migrateliquidity)
  - [receiveLiquidity](#receiveliquidity)
  - [paused](#paused)
  - [superchainConfig](#superchainconfig)
  - [proxyAdminOwner](#proxyadminowner)
  - [proxyAdmin](#proxyadmin)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

Manages unified ETH liquidity for multiple OptimismPortal contracts within a Superchain cluster,
enabling withdrawals from any chain regardless of where ETH was originally deposited.

## Definitions

### Authorized Portal

An OptimismPortal contract that has been granted permission to lock and unlock ETH from the
lockbox. Authorization requires the portal to share the same ProxyAdmin owner and SuperchainConfig
as the lockbox.

### Authorized Lockbox

Another ETHLockbox contract that has been granted permission to migrate its liquidity to the
current lockbox. Authorization requires both lockboxes to share the same ProxyAdmin owner.

### Liquidity Migration

The process of transferring the entire ETH balance from one ETHLockbox to another, typically
performed during system upgrades or when merging multiple lockboxes into a unified liquidity pool.

## Assumptions

### a01-001: ProxyAdmin Owner Trusted

The ProxyAdmin owner is governance-trusted and operates within governance constraints when
authorizing portals and lockboxes, and when initiating [Liquidity Migrations](#liquidity-migration).

#### Mitigations

- Authorization functions are restricted to ProxyAdmin owner only
- Shared ProxyAdmin owner requirement ensures consistent security model across components
- SuperchainConfig consistency check prevents cross-cluster authorization

### a01-002: Authorized Portals Trusted

Approved OptimismPortal contracts are trusted and will only request valid withdrawals of funds.

#### Mitigations

- Portal authorization requires shared ProxyAdmin owner and SuperchainConfig
- Portal authorization is controlled by governance-trusted ProxyAdmin owner

## Invariants

### i01-001: Portal Authorization Required

ETH can only be distributed to approved OptimismPortal contracts.

#### Impact

**Severity: Critical**

If violated, unauthorized contracts could drain the lockbox, resulting in loss of all pooled ETH
across the Superchain cluster.

### i01-002: Always Accessible

[Authorized Portals](#authorized-portal) can always access funds in the lockbox immediately, subject only to pause state.

#### Impact

**Severity: High**

If violated, legitimate withdrawals could be blocked, trapping user funds and breaking the liveness
guarantee of the unified liquidity system.

## Function Specification

### initialize

Initializes the ETHLockbox contract with a SystemConfig reference and initial set of [Authorized Portals](#authorized-portal).

**Parameters:**

- `_systemConfig`: The SystemConfig contract address for pause status and SuperchainConfig
  reference
- `_portals`: Array of OptimismPortal addresses to authorize during initialization

**Behavior:**

- MUST revert if caller is not ProxyAdmin or ProxyAdmin owner
- MUST set the `systemConfig` state variable to `_systemConfig`
- MUST authorize each portal in `_portals` array by calling internal `_authorizePortal` function
- MUST revert if any portal has a different SuperchainConfig than the lockbox
- MUST revert if any portal has a different ProxyAdmin owner than the lockbox
- MUST emit `PortalAuthorized` event for each successfully authorized portal

### lockETH

Accepts ETH deposits from [Authorized Portals](#authorized-portal) and adds them to the unified liquidity pool.

**Behavior:**

- MUST accept ETH value sent with the transaction
- MUST revert if caller is not an [Authorized Portal](#authorized-portal)
- MUST emit `ETHLocked` event with the calling portal address and ETH amount
- MUST NOT revert when paused (locks are permitted during pause, only unlocks are blocked)

### unlockETH

Transfers ETH from the lockbox to an [Authorized Portal](#authorized-portal) to fulfill withdrawal obligations.

**Parameters:**

- `_value`: Amount of ETH to unlock and transfer to the calling portal

**Behavior:**

- MUST revert if the lockbox is paused
- MUST revert if caller is not an [Authorized Portal](#authorized-portal)
- MUST revert if `_value` exceeds the lockbox's ETH balance
- MUST revert if the calling portal's `l2Sender()` is not `DEFAULT_L2_SENDER`
- MUST transfer `_value` ETH to the calling portal using `donateETH()` function
- MUST emit `ETHUnlocked` event with the calling portal address and ETH amount

### authorizePortal

Grants an [Authorized Portal](#authorized-portal) permission to lock and unlock ETH from the lockbox.

**Parameters:**

- `_portal`: Address of the OptimismPortal contract to authorize

**Behavior:**

- MUST revert if caller is not the ProxyAdmin owner
- MUST revert if `_portal` has a different ProxyAdmin owner than the lockbox
- MUST revert if `_portal` has a different SuperchainConfig than the lockbox
- MUST set `authorizedPortals[_portal]` to `true`
- MUST emit `PortalAuthorized` event with the portal address

### authorizeLockbox

Grants another [Authorized Lockbox](#authorized-lockbox) permission to migrate its liquidity to the current lockbox.

**Parameters:**

- `_lockbox`: Address of the ETHLockbox contract to authorize

**Behavior:**

- MUST revert if caller is not the ProxyAdmin owner
- MUST revert if `_lockbox` has a different ProxyAdmin owner than the current lockbox
- MUST set `authorizedLockboxes[_lockbox]` to `true`
- MUST emit `LockboxAuthorized` event with the lockbox address
- MAY be called multiple times for the same lockbox without reverting

### migrateLiquidity

Transfers the entire ETH balance from the current lockbox to another [Authorized Lockbox](#authorized-lockbox).

**Parameters:**

- `_lockbox`: Address of the destination ETHLockbox contract

**Behavior:**

- MUST revert if caller is not the ProxyAdmin owner
- MUST revert if `_lockbox` has a different ProxyAdmin owner than the current lockbox
- MUST capture the current ETH balance before transfer
- MUST call `receiveLiquidity()` on `_lockbox` with the entire ETH balance
- MUST emit `LiquidityMigrated` event with the destination lockbox address and transferred amount
- SHOULD be executed atomically with `OptimismPortal.migrateToSuperRoots()` in the same
  transaction batch

### receiveLiquidity

Receives ETH liquidity from another [Authorized Lockbox](#authorized-lockbox) during migration.

**Behavior:**

- MUST accept ETH value sent with the transaction
- MUST revert if caller is not an [Authorized Lockbox](#authorized-lockbox)
- MUST emit `LiquidityReceived` event with the calling lockbox address and ETH amount

### paused

Returns the current pause status by delegating to the SystemConfig contract.

**Behavior:**

- MUST return the result of `systemConfig.paused()`

### superchainConfig

Returns the SuperchainConfig contract address by delegating to the SystemConfig contract.

**Behavior:**

- MUST return the result of `systemConfig.superchainConfig()`

### proxyAdminOwner

Returns the owner address of the ProxyAdmin that manages this lockbox.

**Behavior:**

- MUST return the result of `proxyAdmin().owner()`

### proxyAdmin

Returns the ProxyAdmin contract that manages this lockbox by reading from EIP-1967 storage slot or
legacy AddressManager.

**Behavior:**

- MUST return the ProxyAdmin address from the EIP-1967 `PROXY_OWNER_ADDRESS` storage slot if
  non-zero
- MUST attempt to read from AddressManager storage if EIP-1967 slot is zero
- MUST revert if no valid ProxyAdmin address can be found
