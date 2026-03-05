# OptimismPortal Interop

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
  - [Integrating `ETHLockbox`](#integrating-ethlockbox)
- [Interface and properties](#interface-and-properties)
  - [ETH Management](#eth-management)
    - [`migrateLiquidity`](#migrateliquidity)
    - [`proxyAdminOwner`](#proxyadminowner)
  - [Internal ETH functionality](#internal-eth-functionality)
    - [Locking ETH](#locking-eth)
    - [Unlocking ETH](#unlocking-eth)
- [Events](#events)
  - [`ETHMigrated`](#ethmigrated)
- [Invariants](#invariants)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `OptimismPortal` contract is integrated with the `ETHLockbox` for managing unified ETH liquidity.
This liquidity consists of every ETH balance migrated from each `OptimismPortal` when joining
the op-governed dependency set.

It is possible to upgrade to this version without being part of the op-governed dependency set. In this case,
the corresponding chain would need to deploy and manage its own `ETHLockbox`.

### Integrating `ETHLockbox`

The integration with the `ETHLockbox` involves locking ETH when executing deposit transactions and unlocking ETH
when finalizing withdrawal transactions, without altering other aspects of the current `OptimismPortal` implementation.

## Interface and properties

### ETH Management

#### `migrateLiquidity`

Migrates the ETH liquidity to the `ETHLockbox`. This function will only be called once by the
`ProxyAdmin` owner when updating the `OptimismPortal` contract.

```solidity
function migrateLiquidity() external;
```

- MUST only be callable by the `ProxyAdmin` owner
- MUST transfer all ETH balance to the `ETHLockbox`
- MUST emit an `ETHMigrated` event with the amount transferred

#### `proxyAdminOwner`

Returns the `ProxyAdmin` owner that manages the `ETHLockbox`.

```solidity
function proxyAdminOwner() external view returns (address);
```

### Internal ETH functionality

#### Locking ETH

Called during deposit transactions to handle ETH locking.

```solidity
if (msg.value > 0) ethLockbox.lockETH{ value: msg.value }();
```

- MUST be invoked during `depositTransaction` when there is ETH value
- MUST lock any ETH value in the `ETHLockbox`

#### Unlocking ETH

Called during withdrawal finalization to handle ETH unlocking.

```solidity
if (_tx.value > 0) ethLockbox.unlockETH(_tx.value);
```

- MUST be invoked during withdrawal finalization when there is ETH value
- MUST unlock the withdrawal value from the `ETHLockbox`
- MUST revert if withdrawal target is the `ETHLockbox`

## Events

### `ETHMigrated`

MUST be triggered when the ETH liquidity is migrated to the `ETHLockbox`.

```solidity
event ETHMigrated(uint256 amount);
```

## Invariants

- Deposits MUST lock the ETH in the `ETHLockbox`

- Withdrawals MUST unlock the ETH from the `ETHLockbox` and forward it to the withdrawal target

- The contract MUST NOT hold any ETH balance from deposits or withdrawals

- The contract MUST be able to handle zero ETH value operations

- The contract MUST NOT allow withdrawals to target the `ETHLockbox` address
