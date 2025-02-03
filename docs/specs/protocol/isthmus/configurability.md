# Configurability

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [`SystemConfig`](#systemconfig)
  - [`ConfigUpdate`](#configupdate)
  - [Initialization](#initialization)
  - [Modifying Operator Fee Parameters](#modifying-operator-fee-parameters)
  - [Interface](#interface)
    - [Operator fee parameters](#operator-fee-parameters)
      - [`operatorFeeScalar`](#operatorfeescalar)
      - [`operatorFeeConstant`](#operatorfeeconstant)
      - [`setOperatorFeeScalars`](#setoperatorfeescalars)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The SystemConfig is updated.

## `SystemConfig`

### `ConfigUpdate`

The following `ConfigUpdate` event is defined where the `CONFIG_VERSION` is `uint256(0)`:

| Name | Value | Definition | Usage |
| ---- | ----- | --- | -- |
| `BATCHER` | `uint8(0)` | `abi.encode(address)` | Modifies the account that is authorized to progress the safe chain |
| `FEE_SCALARS` | `uint8(1)` | `(uint256(0x01) << 248) \| (uint256(_blobbasefeeScalar) << 32) \| _basefeeScalar` | Modifies the fee scalars |
| `GAS_LIMIT` | `uint8(2)` | `abi.encode(uint64 _gasLimit)` | Modifies the L2 gas limit |
| `UNSAFE_BLOCK_SIGNER` | `uint8(3)` | `abi.encode(address)` | Modifies the account that is authorized to progress the unsafe chain |
| `EIP_1559_PARAMS` | `uint8(4)` | `uint256(uint64(uint32(_denominator))) << 32 \| uint64(uint32(_elasticity))` | Modifies the EIP-1559 denominator and elasticity |
| `OPERATOR_FEE_PARAMS` | `uint8(5)` | `uint256(_operatorFeeScalar) << 64 \| _operatorFeeConstant` | Modifies the operator fee sclar and constant |

### Initialization

The following actions should happen during the initialization of the `SystemConfig`:

- `emit ConfigUpdate.BATCHER`
- `emit ConfigUpdate.FEE_SCALARS`
- `emit ConfigUpdate.GAS_LIMIT`
- `emit ConfigUpdate.UNSAFE_BLOCK_SIGNER`
- `emit ConfigUpdate.EIP_1559_PARAMS`

These actions MAY only be triggered if there is a diff to the value.

The `operatorFeeScalar` and `operatorFeeConstant` are initialized to 0.

### Modifying Operator Fee Parameters

A new `SystemConfig` `UpdateType` is introduced that enables the modification of
the `operatorFeeScalar` and `operatorFeeConstant` by the `SystemConfig` owner.

### Interface

#### Operator fee parameters

##### `operatorFeeScalar`

This function returns the currently configured operator fee scalar.

```solidity
function operatorFeeScalar()(uint32)
```

##### `operatorFeeConstant`

This function returns the currently configured operator fee constant.

```solidity
function operatorFeeConstant()(uint64)
```

##### `setOperatorFeeScalars`

This function sets the `operatorFeeScalar` and `operatorFeeConstant`.

This function MUST only be callable by the `SystemConfig` owner.

```solidity
function setOperatorFeeScalar(uint32 _operatorFeeScalar, uint64 _operatorFeeConstant)
```
