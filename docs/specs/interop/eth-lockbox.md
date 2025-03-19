# ETH Lockbox

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Design](#design)
  - [Interface and properties](#interface-and-properties)
    - [`lockETH`](#locketh)
    - [`unlockETH`](#unlocketh)
    - [`authorizePortal`](#authorizeportal)
    - [`authorizeLockbox`](#authorizelockbox)
    - [`migrateLiquidity`](#migrateliquidity)
    - [`receiveLiquidity`](#receiveliquidity)
    - [`proxyAdminOwner`](#proxyadminowner)
  - [Events](#events)
    - [`ETHLocked`](#ethlocked)
    - [`ETHUnlocked`](#ethunlocked)
    - [`PortalAuthorized`](#portalauthorized)
    - [`LockboxAuthorized`](#lockboxauthorized)
    - [`LiquidityMigrated`](#liquiditymigrated)
    - [`LiquidityReceived`](#liquidityreceived)
- [Invariants](#invariants)
  - [System level invariants](#system-level-invariants)
  - [Contract level invariants](#contract-level-invariants)
- [Architecture](#architecture)
  - [ETH Management](#eth-management)
  - [Merge process](#merge-process)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

With interoperable ETH, withdrawals will fail if the referenced `OptimismPortal` lacks sufficient ETH.
This is due to having the possibility to move ETH liquidity across the different chains and it could happen
that a chain ends up with more liquidity than its `OptimismPortal`.
The `ETHLockbox` improves the Superchain's interoperable ETH withdrawal user experience and avoids this issue.
To do so, it unifies ETH L1 liquidity in a single contract (`ETHLockbox`), enabling seamless withdrawals of ETH
from any OP chain in the Superchain, regardless of where the ETH was initially deposited.

## Design

The `ETHLockbox` contract is designed to manage the unified ETH liquidity for the Superchain.
It implements two main functions: `lockETH` for depositing ETH into the lockbox,
and `unlockETH` for withdrawing ETH from the lockbox.

These functions are called by the `OptimismPortal` contracts to manage the shared ETH liquidity
when making deposits or finalizing withdrawals.

Authorization of `OptimismPortal`s is managed by the `ProxyAdmin` owner.
The `ETHLockbox` contract is proxied and managed by the L1 `ProxyAdmin`.

### Interface and properties

#### `lockETH`

Deposits and locks ETH into the lockbox's liquidity pool.

- The function MUST accept ETH.
- Only authorized `OptimismPortal` addresses MUST be allowed to interact.
- The function MUST NOT revert when called by an authorized `OptimismPortal`
- The function MUST emit the `ETHLocked` event with the `portal` that called it and the `amount`.

```solidity
function lockETH() external payable;
```

#### `unlockETH`

Withdraws a specified amount of ETH from the lockbox's liquidity pool to the `OptimismPortal` calling it.

- Only authorized `OptimismPortal` addresses MUST be allowed to interact.
- The function MUST NOT revert when called by an authorized `OptimismPortal` unless paused.
- The function MUST emit the `ETHUnlocked` event with the `portal` that called it and the `amount`.
- The function MUST use `donateETH` when sending ETH to avoid triggering deposits.
- The function MUST NOT allow to be called as part of a withdrawal transaction (`OptimismPortal.l2Sender()` MUST be the `DEFAULT_L2_SENDER`).

```solidity
function unlockETH(uint256 _value) external;
```

#### `authorizePortal`

Authorizes an `OptimismPortal` to interact with the `ETHLockbox`.

- Only the `ProxyAdmin` owner can call the function.
- The `ProxyAdmin` owner of the `OptimismPortal` must be the same as the `ProxyAdmin` owner of the `ETHLockbox`.
- The `OptimismPortal` and `ETHLockbox` MUST share the same `SuperchainConfig` address
- The function MUST emit the `PortalAuthorized` event with the `portal`.

```solidity
function authorizePortal(address _portal) external;
```

#### `authorizeLockbox`

Authorizes another `ETHLockbox` to migrate its ETH liquidity to the current `ETHLockbox`.

- Only the `ProxyAdmin` owner can call the function.
- The `ProxyAdmin` owner of the source lockbox must be the same as the `ProxyAdmin` owner of the destination lockbox.
- The function MUST emit the `LockboxAuthorized` event with the `lockbox` that is being authorized.

```solidity
function authorizeLockbox(address _lockbox) external;
```

#### `migrateLiquidity`

Migrates the ETH liquidity from the current `ETHLockbox` to another `ETHLockbox`.

- Only the `ProxyAdmin` owner can call the function.
- The `ProxyAdmin` owner of the source lockbox must be the same as the `ProxyAdmin` owner of the destination lockbox.
- The function MUST call `receiveLiquidity` from the destination `ETHLockbox`.
- The function MUST emit the `LiquidityMigrated` event with the `lockbox` that is being migrated to.

```solidity
function migrateLiquidity(address _lockbox) external;
```

#### `receiveLiquidity`

Receives the ETH liquidity from another `ETHLockbox`.

- Only an authorized `ETHLockbox` can call the function.
- The function MUST emit the `LiquidityReceived` event with the `lockbox` that is being received from.

```solidity
function receiveLiquidity() external payable;
```

#### `proxyAdminOwner`

Returns the `ProxyAdmin` owner that manages the `ETHLockbox`.

```solidity
function proxyAdminOwner() external view returns (address);
```

### Events

#### `ETHLocked`

MUST be triggered when `lockETH` is called

```solidity
event ETHLocked(address indexed portal, uint256 amount);
```

#### `ETHUnlocked`

MUST be triggered when `unlockETH` is called

```solidity
event ETHUnlocked(address indexed portal, uint256 amount);
```

#### `PortalAuthorized`

MUST be triggered when `authorizePortal` is called

```solidity
event PortalAuthorized(address indexed portal);
```

#### `LockboxAuthorized`

MUST be triggered when `authorizeLockbox` is called

```solidity
event LockboxAuthorized(address indexed lockbox);
```

#### `LiquidityMigrated`

MUST be triggered when `migrateLiquidity` is called

```solidity
event LiquidityMigrated(address indexed lockbox);
```

#### `LiquidityReceived`

MUST be triggered when `receiveLiquidity` is called

```solidity
event LiquidityReceived(address indexed lockbox);
```

## Invariants

### System level invariants

- The ETH held in the `ETHLockbox` MUST never be less than the amount deposited but not yet withdrawn by the `OptimismPortal`s

- All chains joining the same `ETHLockbox` MUST have the same `ProxyAdmin` owner

- The total withdrawable ETH amount present on all the dependency set's chains MUST NEVER be more than the amount held
  by the `ETHLockbox` of the cluster
  > With "withdrawable amount", the ETH balance held on `ETHLiquidity` is excluded

### Contract level invariants

- It MUST allow only authorized portals to lock ETH

- It MUST allow only authorized portals to unlock ETH

- It MUST be in paused state if the `SuperchainConfig` is paused

- No Ether MUST flow out of the contract when in a paused state

- It MUST NOT trigger a new deposit when ETH amount is being unlocked from the `ETHLockbox` by the `OptimismPortal`

- It MUST allow only the `ProxyAdmin` owner to call the `authorizePortal`, `authorizeLockbox` and `migrateLiquidity` functions

- It MUST allow only authorized lockboxes to call the `receiveLiquidity` function

- It MUST migrate the whole ETH liquidity from the source `ETHLockbox` to the destination `ETHLockbox` when calling `migrateLiquidity`

- It MUST emit:

  - An `ETHLocked` event when locking ETH

  - An `ETHUnlocked` event when unlocking ETH

  - A `PortalAuthorized` event when authorizing a portal

  - A `LockboxAuthorized` event when authorizing a lockbox

  - A `LiquidityMigrated` event when migrating liquidity

  - A `LiquidityReceived` event when receiving liquidity

## Architecture

### ETH Management

- ETH is locked in the `ETHLockbox` when:

  - A portal migrates its ETH liquidity when updating

  - A deposit is made with ETH value on an authorized portal

- ETH is unlocked from the `ETHLockbox` when:

  - An authorized portal finalizes a withdrawal that requires ETH

### Merge process

The merge process is the process of merging two `ETHLockbox`es into a single one,
transferring the ETH liquidity from the both source `ETHLockbox` to the destination `ETHLockbox`.

For each source `ETHLockbox`, the following steps MUST be followed:

- The destination `ETHLockbox` MUST call `authorizeLockbox` with the source `ETHLockbox` as argument.

- The source `ETHLockbox` MUST call `migrateLiquidity` with the destination `ETHLockbox` as argument.

- `migrateLiquidity` MUST call `receiveLiquidity` from the destination `ETHLockbox`.

This process ensures that the ETH liquidity is migrated from the source `ETHLockbox` to the correct destination `ETHLockbox`.

These transactions SHOULD be executed atomically. A possible way is through the `OPCM` contract.
