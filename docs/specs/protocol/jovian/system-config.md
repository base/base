# Jovian: System Config

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Minimum Base Fee Configuration](#minimum-base-fee-configuration)
  - [`ConfigUpdate`](#configupdate)
  - [Initialization](#initialization)
  - [Modifying Minimum Base Fee](#modifying-minimum-base-fee)
  - [Interface](#interface)
    - [Minimum Base Fee Parameters](#minimum-base-fee-parameters)
      - [`minBaseFee`](#minbasefee)
- [DA Footprint Configuration](#da-footprint-configuration)
  - [`ConfigUpdate`](#configupdate-1)
  - [Modifying DA Footprint Gas Scalar](#modifying-da-footprint-gas-scalar)
  - [Interface](#interface-1)
    - [DA Footprint Gas Scalar Parameters](#da-footprint-gas-scalar-parameters)
      - [`daFootprintGasScalar`](#dafootprintgasscalar)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Minimum Base Fee Configuration

Jovian adds a configuration value to `SystemConfig` to control the minimum base fee used by the EIP-1559 fee market
on OP Stack chains. The value is a minimum base fee in wei.

| Name         | Type     | Default | Meaning                 |
|--------------|----------|---------|-------------------------|
| `minBaseFee` | `uint64` | `0`     | Minimum base fee in wei |

The configuration is updated via a new method on `SystemConfig`:

```solidity
function setMinBaseFee(uint64 minBaseFee) external onlyOwner;
```

### `ConfigUpdate`

When the configuration is updated, a [`ConfigUpdate`](../system-config.md#system-config-updates) event
MUST be emitted with the following parameters:

| `version` | `updateType` | `data` | Usage |
| ---- | ----- | --- | -- |
| `uint256(0)` | `uint8(6)` | `abi.encode(uint64(_minBaseFee))` | Modifies the minimum base fee (wei) |

### Initialization

The following actions should happen during the initialization of the `SystemConfig`:

- `emit ConfigUpdate.BATCHER`
- `emit ConfigUpdate.FEE_SCALARS`
- `emit ConfigUpdate.GAS_LIMIT`
- `emit ConfigUpdate.UNSAFE_BLOCK_SIGNER`

Intentionally absent from this is `emit ConfigUpdate.EIP_1559_PARAMS` and `emit ConfigUpdate.MIN_BASE_FEE`.
As long as these values are unset, the default values will be used.
Requiring these parameters to be set during initialization would add a strict requirement
that the L2 hardforks before the L1 contracts are upgraded, and this is complicated to manage in a
world of many chains.

### Modifying Minimum Base Fee

Upon update, the contract emits the `ConfigUpdate` event above, enabling nodes
to derive the configuration from L1 logs.

Implementations MUST incorporate the configured value into the block header `extraData` as specified in
`./exec-engine.md`. Until the first such event is emitted, a default value of `0` should be used.

### Interface

#### Minimum Base Fee Parameters

##### `minBaseFee`

This function returns the currently configured minimum base fee in wei.

```solidity
function minBaseFee() external view returns (uint64);
```

## DA Footprint Configuration

Jovian adds a `uint16` configuration value to `SystemConfig` to control the [`daFootprintGasScalar`](./derivation.md).

The configuration is updated via a new method on `SystemConfig`:

```solidity
function setDAFootprintGasScalar(uint16 daFootprintGasScalar) external onlyOwner;
```

### `ConfigUpdate`

When the configuration is updated, a [`ConfigUpdate`](../system-config.html#system-config-updates) event
MUST be emitted with the following parameters:

| `version` | `updateType` | `data` | Usage |
| ---- | ----- | --- | -- |
| `uint256(0)` | `uint8(7)` | `abi.encode(uint16(_daFootprintGasScalar))` | Modifies the DA footprint gas scalar |

### Modifying DA Footprint Gas Scalar

Upon update, the contract emits the `ConfigUpdate` event above, enabling nodes
to derive the configuration from L1 logs.

### Interface

#### DA Footprint Gas Scalar Parameters

##### `daFootprintGasScalar`

This function returns the currently configured DA footprint gas scalar.

```solidity
function daFootprintGasScalar() external view returns (uint16);
```
