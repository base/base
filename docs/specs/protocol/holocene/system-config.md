# System Config

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
  - [`ConfigUpdate`](#configupdate)
  - [Initialization](#initialization)
  - [Modifying EIP-1559 Parameters](#modifying-eip-1559-parameters)
  - [Interface](#interface)
    - [EIP-1559 Params](#eip-1559-params)
      - [`setEIP1559Params`](#seteip1559params)
      - [`eip1559Elasticity`](#eip1559elasticity)
      - [`eip1559Denominator`](#eip1559denominator)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The `SystemConfig` is updated to allow for dynamic EIP-1559 parameters.

### `ConfigUpdate`

The following `ConfigUpdate` event is defined where the `CONFIG_VERSION` is `uint256(0)`:

| Name | Value | Definition | Usage |
| ---- | ----- | --- | -- |
| `BATCHER` | `uint8(0)` | `abi.encode(address)` | Modifies the account that is authorized to progress the safe chain |
| `FEE_SCALARS` | `uint8(1)` | `abi.encode(uint256(0), (uint256(0x01) << 248) \| (uint256(_blobbasefeeScalar) << 32) \| _basefeeScalar)` | Modifies the fee scalars |
| `GAS_LIMIT` | `uint8(2)` | `abi.encode(uint64 _gasLimit)` | Modifies the L2 gas limit |
| `UNSAFE_BLOCK_SIGNER` | `uint8(3)` | `abi.encode(address)` | Modifies the account that is authorized to progress the unsafe chain |
| `EIP_1559_PARAMS` | `uint8(4)` | `abi.encode((uint256(_denominator) << 32) \| _elasticity)` | Modifies the EIP-1559 denominator and elasticity |

Note that the above encoding is the format emitted by the SystemConfig event, which differs from the format in extraData
from the block header.

### Initialization

The following actions should happen during the initialization of the `SystemConfig`:

- `emit ConfigUpdate.BATCHER`
- `emit ConfigUpdate.FEE_SCALARS`
- `emit ConfigUpdate.GAS_LIMIT`
- `emit ConfigUpdate.UNSAFE_BLOCK_SIGNER`
- `emit ConfigUpdate.EIP_1559_PARAMS`

### Modifying EIP-1559 Parameters

A new `SystemConfig` `UpdateType` is introduced that enables the modification of
[EIP-1559](https://eips.ethereum.org/EIPS/eip-1559) parameters. This allows for the chain
operator to modify the `BASE_FEE_MAX_CHANGE_DENOMINATOR` and the `ELASTICITY_MULTIPLIER`.

### Interface

#### EIP-1559 Params

##### `setEIP1559Params`

This function MUST only be callable by the chain governor.

```solidity
function setEIP1559Params(uint32 _denominator, uint32 _elasticity)
```

The `_denominator` and `_elasticity` MUST be set to values greater to than 0.
It is possible for the chain operator to set EIP-1559 parameters that result in poor user experience.

##### `eip1559Elasticity`

This function returns the currently configured EIP-1559 elasticity.

```solidity
function eip1559Elasticity()(uint32)
```

##### `eip1559Denominator`

This function returns the currently configured EIP-1559 denominator.

```solidity
function eip1559Denominator()(uint32)
```
