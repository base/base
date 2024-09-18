# Configurability

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Constants](#constants)
  - [`ConfigType`](#configtype)
- [`SystemConfig`](#systemconfig)
  - [`ConfigUpdate`](#configupdate)
  - [Initialization](#initialization)
  - [Modifying EIP-1559 Parameters](#modifying-eip-1559-parameters)
  - [Interface](#interface)
    - [EIP-1559 Params](#eip-1559-params)
      - [`setEIP1559Params`](#seteip1559params)
    - [Fee Vault Config](#fee-vault-config)
      - [`setBaseFeeVaultConfig`](#setbasefeevaultconfig)
      - [`setL1FeeVaultConfig`](#setl1feevaultconfig)
      - [`setSequencerFeeVaultConfig`](#setsequencerfeevaultconfig)
- [`OptimismPortal`](#optimismportal)
  - [Interface](#interface-1)
    - [`setConfig`](#setconfig)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `SystemConfig` and `OptimismPortal` are updated with a new flow for chain
configurability.

## Constants

### `ConfigType`

The `ConfigType` enum represents configuration that can be modified.

| Name | Value | Description |
| ---- | ----- | --- |
| `SET_GAS_PAYING_TOKEN` | `uint8(0)` | Modifies the gas paying token for the chain |
| `SET_BASE_FEE_VAULT_CONFIG` | `uint8(1)` | Sets the Fee Vault Config for the `BaseFeeVault` |
| `SET_L1_FEE_VAULT_CONFIG` | `uint8(2)` | Sets the Fee Vault Config for the `L1FeeVault` |
| `SET_SEQUENCER_FEE_VAULT_CONFIG` | `uint8(3)` | Sets the Fee Vault Config for the `SequencerFeeVault` |
| `SET_L1_CROSS_DOMAIN_MESSENGER_ADDRESS` | `uint8(4)` | Sets the `L1CrossDomainMessenger` address |
| `SET_L1_ERC_721_BRIDGE_ADDRESS` | `uint8(5)` | Sets the `L1ERC721Bridge` address |
| `SET_L1_STANDARD_BRIDGE_ADDRESS` | `uint8(6)` | Sets the `L1StandardBridge` address |
| `SET_REMOTE_CHAIN_ID` | `uint8(7)` | Sets the chain id of the base chain |

## `SystemConfig`

### `ConfigUpdate`

The following `ConfigUpdate` enum is defined where the `CONFIG_VERSION` is `uint256(0)`:

| Name | Value | Definition | Usage |
| ---- | ----- | --- | -- |
| `BATCHER` | `uint8(0)` | `abi.encode(address)` | Modifies the account that is authorized to progress the safe chain |
| `FEE_SCALARS` | `uint8(1)` | `(uint256(0x01) << 248) \| (uint256(_blobbasefeeScalar) << 32) \| _basefeeScalar` | Modifies the fee scalars |
| `GAS_LIMIT` | `uint8(2)` | `abi.encode(uint64 _gasLimit)` | Modifies the L2 gas limit |
| `UNSAFE_BLOCK_SIGNER` | `uint8(3)` | `abi.encode(address)` | Modifies the account that is authorized to progress the unsafe chain |
| `EIP_1559_PARAMS` | `uint8(4)` | `uint256(uint64(_denominator)) << 32 | uint64(_elasticity)` | Modifies the EIP-1559 denominator and elasticity |

### Initialization

The following actions should happen during the initialization of the `SystemConfig`:

- `emit ConfigUpdate.BATCHER`
- `emit ConfigUpdate.FEE_SCALARS`
- `emit ConfigUpdate.GAS_LIMIT`
- `emit ConfigUpdate.UNSAFE_BLOCK_SIGNER`
- `emit ConfigUpdate.EIP_1559_PARAMS`
- `setConfig(SET_GAS_PAYING_TOKEN)`
- `setConfig(SET_BASE_FEE_VAULT_CONFIG)`
- `setConfig(SET_L1_FEE_VAULT_CONFIG)`
- `setConfig(SET_SEQUENCER_FEE_VAULT_CONFIG)`
- `setConfig(SET_L1_CROSS_DOMAIN_MESSENGER_ADDRESS)`
- `setConfig(SET_L1_ERC_721_BRIDGE_ADDRESS)`
- `setConfig(SET_L1_STANDARD_BRIDGE_ADDRESS)`
- `setConfig(SET_REMOTE_CHAIN_ID)`

These actions MAY only be triggered if there is a diff to the value.

### Modifying EIP-1559 Parameters

A new `SystemConfig` `UpdateType` is introduced that enables the modification of
[EIP-1559](https://eips.ethereum.org/EIPS/eip-1559) parameters. This allows for the chain
operator to modify the `BASE_FEE_MAX_CHANGE_DENOMINATOR` and the `ELASTICITY_MULTIPLIER`.

### Interface

#### EIP-1559 Params

##### `setEIP1559Params`

This function MUST only be callable by the chain governor.

```solidity
function setEIP1559Params(uint64 _denominator, uint64 _elasticity)
```

The `_denominator` and `_elasticity` MUST be set to values greater to than 0.
It is possible for the chain operator to set EIP-1559 parameters that result in poor user experience.

#### Fee Vault Config

For each `FeeVault`, there is a setter for its config. The arguments to the setter include
the `RECIPIENT`, the `MIN_WITHDRAWAL_AMOUNT` and the `WithdrawalNetwork`.
Each of these functions should be `public` and only callable by the chain governor.

Each function calls `OptimismPortal.setConfig(ConfigType,bytes)` with its corresponding `ConfigType`.

##### `setBaseFeeVaultConfig`

```solidity
function setBaseFeeVaultConfig(address,uint256,WithdrawalNetwork)
```

##### `setL1FeeVaultConfig`

```solidity
function setL1FeeVaultConfig(address,uint256,WithdrawalNetwork)
```

##### `setSequencerFeeVaultConfig`

```solidity
function setSequencerFeeVaultConfig(address,uint256,WithdrawalNetwork)
```

## `OptimismPortal`

The `OptimismPortal` is updated to emit a special system `TransactionDeposited` event.

### Interface

#### `setConfig`

The `setConfig` function MUST only be callable by the `SystemConfig`. This ensures that the `SystemConfig`
is the single source of truth for chain operator ownership.

```solidity
function setConfig(ConfigType,bytes)
```

This function emits a `TransactionDeposited` event.

```solidity
event TransactionDeposited(address indexed from, address indexed to, uint256 indexed version, bytes opaqueData);
```

The following fields are included:

- `from` is the `DEPOSITOR_ACCOUNT`
- `to` is `Predeploys.L1Block`
- `version` is `uint256(0)`
- `opaqueData` is the tightly packed transaction data where `mint` is `0`, `value` is `0`, the `gasLimit`
   is `200_000`, `isCreation` is `false` and the `data` is `abi.encodeCall(L1Block.setConfig, (_type, _value))`
