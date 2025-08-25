# Jovian: System Config

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Minimum Base Fee Configuration](#minimum-base-fee-configuration)
  - [`ConfigUpdate`](#configupdate)
  - [Modifying Minimum Base Fee](#modifying-minimum-base-fee)
  - [Interface](#interface)
    - [Minimum Base Fee Parameters](#minimum-base-fee-parameters)
      - [`minBaseFee`](#minbasefee)

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

The following `ConfigUpdate` event is defined where the `CONFIG_VERSION` is `uint256(0)`:

| Name | Value | Definition | Usage |
| ---- | ----- | --- | -- |
| `BATCHER` | `uint8(0)` | `abi.encode(address)` | Modifies the account that is authorized to progress the safe chain |
| `FEE_SCALARS` | `uint8(1)` | `(uint256(0x01) << 248) \| (uint256(_blobbasefeeScalar) << 32) \| _basefeeScalar` | Modifies the fee scalars |
| `GAS_LIMIT` | `uint8(2)` | `abi.encode(uint64 _gasLimit)` | Modifies the L2 gas limit |
| `UNSAFE_BLOCK_SIGNER` | `uint8(3)` | `abi.encode(address)` | Modifies the account that is authorized to progress the unsafe chain |
| `EIP_1559_PARAMS` | `uint8(4)` | `uint256(uint64(uint32(_denominator))) << 32 \| uint64(uint32(_elasticity))` | Modifies the EIP-1559 denominator and elasticity |
| `OPERATOR_FEE_PARAMS` | `uint8(5)` | `uint256(_operatorFeeScalar) << 64 \| _operatorFeeConstant` | Modifies the operator fee scalar and constant |
| `MIN_BASE_FEE` | `uint8(6)` | `abi.encode(uint64(_minBaseFee))` | Modifies the minimum base fee (wei) |

### Modifying Minimum Base Fee

Upon update, the contract emits a `ConfigUpdate` event with a new `UpdateType` value `MIN_BASE_FEE`, enabling nodes
to derive the configuration from L1 logs.

Implementations MUST incorporate the configured value into the block header `extraData` as specified in
`./exec-engine.md`.

### Interface

#### Minimum Base Fee Parameters

##### `minBaseFee`

This function returns the currently configured minimum base fee in wei.

```solidity
function minBaseFee() external view returns (uint64);
```
