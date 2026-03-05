# Predeploys

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [WETH9](#weth9)
- [L1Block](#l1block)
  - [Functions](#functions)
    - [`setCustomGasToken`](#setcustomgastoken)
- [L2CrossDomainMessenger](#l2crossdomainmessenger)
- [L2StandardBridge](#l2standardbridge)
- [SequencerFeeVault](#sequencerfeevault)
- [BaseFeeVault](#basefeevault)
- [L1FeeVault](#l1feevault)
- [Native Asset Liquidity](#native-asset-liquidity)
  - [Functions](#functions-1)
    - [`deposit`](#deposit)
    - [`withdraw`](#withdraw)
  - [Events](#events)
    - [`LiquidityDeposited`](#liquiditydeposited)
    - [`LiquidityWithdrawn`](#liquiditywithdrawn)
  - [Invariants](#invariants)
- [Liquidity Controller](#liquidity-controller)
  - [Functions](#functions-2)
    - [`authorizeMinter`](#authorizeminter)
    - [`deauthorizeMinter`](#deauthorizeminter)
    - [`mint`](#mint)
    - [`burn`](#burn)
    - [`gasPayingAssetName`](#gaspayingassetname)
    - [`gasPayingAssetSymbol`](#gaspayingassetsymbol)
  - [Events](#events-1)
    - [`MinterAuthorized`](#minterauthorized)
    - [`MinterDeauthorized`](#minterdeauthorized)
    - [`LiquidityMinted`](#liquidityminted)
    - [`LiquidityBurned`](#liquidityburned)
  - [Invariants](#invariants-1)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

| Name                 | Address                                    | Introduced | Deprecated | Proxied |
| -------------------- | ------------------------------------------ | ---------- | ---------- | ------- |
| NativeAssetLiquidity | 0x4200000000000000000000000000000000000029 | Jovian     | No         | Yes     |
| LiquidityController  | 0x420000000000000000000000000000000000002A | Jovian     | No         | Yes     |

## WETH9

On chains using Custom Gas Token mode, this contract serves as the Wrapped Native
Asset (WNA) instead of Wrapped Ether. The WETH predeploy implementation remains
unchanged and continues to fetch metadata from the `L1Block` predeploy. However,
the `L1Block` predeploy is updated to source the name and symbol from the
`LiquidityController` instead of using hardcoded ETH values, with the name and
symbol being prefixed with "Wrapped" and "W" respectively.

**Important**: Currently, a chain using ETH cannot migrate to use a custom gas token,
as this migration would introduce significant risks and breaking changes to existing
infrastructure. Custom Gas Token mode is only supported for fresh deployments.
Migration only focuses on chains utilizing the old CGT implementation.

For fresh Custom Gas Token deployments, the `L1Block` is deployed from genesis
with the metadata functions already configured to fetch from the `LiquidityController`.

## L1Block

### Functions

#### `setCustomGasToken`

Sets the Custom Gas Token flag to `true`, enabling Custom Gas Token mode for the chain.

```solidity
function setCustomGasToken() external
```

- MUST only be callable by the `DEPOSITOR_ACCOUNT`
- MUST set the internal `isCustomGasToken` flag to `true`
- MUST be callable only once per chain (the flag cannot be reverted to `false`)

Once enabled, various predeploys will check `L1Block.isCustomGasToken()` to determine if ETH
bridging operations should be blocked.

## L2CrossDomainMessenger

The `sendMessage` function MUST revert if `L1Block.isCustomGasToken()` returns `true` and `msg.value > 0`.
This revert occurs because `L2CrossDomainMessenger` internally calls `L2ToL1MessagePasser.initiateWithdrawal`
which enforces the CGT restriction.

## L2StandardBridge

ETH bridging functions MUST revert if `L1Block.isCustomGasToken()` returns `true` and the function involves ETH transfers.
This revert occurs because `L2StandardBridge` internally calls `L2CrossDomainMessenger.sendMessage`,
which in turn calls `L2ToL1MessagePasser.initiateWithdrawal` that enforces the CGT restriction.

## SequencerFeeVault

The contract constructor takes a `WithdrawalNetwork` parameter that determines whether
funds are sent to L1 or L2:

- `WithdrawalNetwork.L1`: Funds are withdrawn to an L1 address (default behavior)
- `WithdrawalNetwork.L2`: Funds are withdrawn to an L2 address

For existing deployments, to change the withdrawal network or recipient address,
a new implementation must be deployed and the proxy must be upgraded.

For fresh deployments with Custom Gas Token mode enabled, the withdrawal network
MUST be set to `WithdrawalNetwork.L2`.

## BaseFeeVault

The contract constructor takes a `WithdrawalNetwork` parameter that determines whether
funds are sent to L1 or L2:

- `WithdrawalNetwork.L1`: Funds are withdrawn to an L1 address (default behavior)
- `WithdrawalNetwork.L2`: Funds are withdrawn to an L2 address

For existing deployments, to change the withdrawal network or recipient address,
a new implementation must be deployed and the proxy must be upgraded.

For fresh deployments with Custom Gas Token mode enabled, the withdrawal network
MUST be set to `WithdrawalNetwork.L2`.

## L1FeeVault

The contract constructor takes a `WithdrawalNetwork` parameter that determines whether
funds are sent to L1 or L2:

- `WithdrawalNetwork.L1`: Funds are withdrawn to an L1 address (default behavior)
- `WithdrawalNetwork.L2`: Funds are withdrawn to an L2 address

For existing deployments, to change the withdrawal network or recipient address,
a new implementation must be deployed and the proxy must be upgraded.

For fresh deployments with Custom Gas Token mode enabled, the withdrawal network
MUST be set to `WithdrawalNetwork.L2`.

## Native Asset Liquidity

Address: `0x4200000000000000000000000000000000000029`

The `NativeAssetLiquidity` predeploy stores a large amount of pre-minted native asset
that serves as the central liquidity source for Custom Gas Token chains. This contract
is only deployed on chains using Custom Gas Token mode and acts as the vault for all
native asset supply management operations. The contract provides controlled access to
liquidity through the `LiquidityController` predeploy only.

### Functions

#### `deposit`

Accepts native asset deposits and locks them in the contract, reducing circulating supply.

```solidity
function deposit() external payable
```

- MUST only be callable by the `LiquidityController` predeploy
- MUST accept any amount of native asset via `msg.value`
- MUST emit `LiquidityDeposited` event

#### `withdraw`

Sends native assets from the contract to the `LiquidityController`, increasing circulating supply.

```solidity
function withdraw(uint256 _amount) external
```

- MUST only be callable by the `LiquidityController` predeploy
- MUST send exactly `_amount` of native asset to the caller
- MUST revert if the contract balance is insufficient
- MUST emit `LiquidityWithdrawn` event

### Events

#### `LiquidityDeposited`

Emitted when native assets are deposited into the contract.

```solidity
event LiquidityDeposited(address indexed caller, uint256 value)
```

#### `LiquidityWithdrawn`

Emitted when native assets are withdrawn from the contract.

```solidity
event LiquidityWithdrawn(address indexed caller, uint256 value)
```

### Invariants

- Only the `LiquidityController` predeploy can call `deposit()` and `withdraw()`
- All native asset supply changes must go through this contract when CGT mode is active
- No direct user interaction is permitted with liquidity management functions

## Liquidity Controller

Address: `0x420000000000000000000000000000000000002A`

The `LiquidityController` predeploy manages access to the `NativeAssetLiquidity` contract
and provides the governance interface for Custom Gas Token chains. This contract is only
deployed on chains using Custom Gas Token mode and serves as the central authority for
native asset supply management and metadata provisioning.

### Functions

#### `authorizeMinter`

Authorizes an address to mint native assets from the liquidity pool.

```solidity
function authorizeMinter(address _minter) external
```

- MUST only be callable by the L1 ProxyAdmin owner
- MUST authorize `_minter` to call the `mint()` function
- MUST emit `MinterAuthorized` event

#### `deauthorizeMinter`

Deauthorizes an address from minting native assets from the liquidity pool.

```solidity
function deauthorizeMinter(address _minter) external
```

- MUST only be callable by the L1 ProxyAdmin owner
- MUST deauthorize `_minter` from calling the `mint()` function
- MUST emit `MinterDeauthorized` event

#### `mint`

Unlocks native assets from the `NativeAssetLiquidity` contract and sends them to a specified address.

```solidity
function mint(address _to, uint256 _amount) external
```

- MUST only be callable by authorized minters
- MUST call `NativeAssetLiquidity.withdraw(_amount)` to unlock assets
- MUST send exactly `_amount` of native asset to `_to` address
- MUST revert if `NativeAssetLiquidity` has insufficient balance
- MUST emit `LiquidityMinted` event

#### `burn`

Deposits native assets back into the `NativeAssetLiquidity` contract, reducing circulating supply.

```solidity
function burn() external payable
```

- MUST accept any amount of native asset via `msg.value`
- MUST call `NativeAssetLiquidity.deposit{value: msg.value}()` to lock assets
- MUST only be callable by authorized minters
- MUST revert if `msg.value` is zero
- MUST emit `LiquidityBurned` event

#### `gasPayingAssetName`

Returns the human-readable name of the gas-paying asset for metadata purposes.

```solidity
function gasPayingAssetName() external view returns (string memory)
```

- MUST return the name of the native asset (e.g., "MyToken")
- MUST be used by L1Block predeploy for `name()` function
- Returns value used to construct "Wrapped {AssetName}" for WNA

#### `gasPayingAssetSymbol`

Returns the symbol of the gas-paying asset for metadata purposes.

```solidity
function gasPayingAssetSymbol() external view returns (string memory)
```

- MUST return the symbol of the native asset (e.g., "MTK")
- MUST be used by L1Block predeploy for `symbol()` function
- Returns value used to construct "W{AssetSymbol}" for WNA

### Events

#### `MinterAuthorized`

Emitted when a new minter is authorized by the contract owner.

```solidity
event MinterAuthorized(address indexed minter)
```

Where `minter` is the address being authorized.

#### `MinterDeauthorized`

Emitted when a minter is deauthorized by the contract owner.

```solidity
event MinterDeauthorized(address indexed minter)
```

Where `minter` is the address being deauthorized.

#### `LiquidityMinted`

Emitted when native assets are unlocked from the liquidity pool and sent to a recipient.

```solidity
event LiquidityMinted(address indexed minter, address indexed to, uint256 amount)
```

Where `minter` is the authorized address calling the function, `to` is the recipient address,
and `amount` is the amount of native assets minted.

#### `LiquidityBurned`

Emitted when native assets are locked back into the liquidity pool.

```solidity
event LiquidityBurned(address indexed minter, uint256 amount)
```

Where `minter` is the `msg.sender` who burned the assets and `amount` is the amount of native assets burned.

### Invariants

- Only authorized minters can call `mint()` to unlock native assets
- Only the L1 ProxyAdmin owner can authorize new minters via `authorizeMinter()`
- All native asset supply changes must flow through this contract's governance
- The contract acts as the sole interface between governance and `NativeAssetLiquidity`
- `burn()` operations always increase locked supply by calling `NativeAssetLiquidity.deposit()`
- `mint()` operations always decrease locked supply by calling `NativeAssetLiquidity.withdraw()`
