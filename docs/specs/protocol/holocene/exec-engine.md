# L2 Execution Engine

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Timestamp Activation](#timestamp-activation)
- [Dynamic EIP-1559 Parameters](#dynamic-eip-1559-parameters)
  - [EIP-1559 Parameters in Block Header](#eip-1559-parameters-in-block-header)
  - [EIP-1559 Parameters in `PayloadAttributesV3`](#eip-1559-parameters-in-payloadattributesv3)
    - [Encoding](#encoding)
    - [PayloadID computation](#payloadid-computation)
  - [Execution](#execution)
    - [Payload Attributes Processing](#payload-attributes-processing)
    - [Base Fee Computation](#base-fee-computation)
  - [Rationale](#rationale)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The EIP-1559 parameters are encoded in the block header's `extraData` field and can be configured dynamically through
the `SystemConfig`.

## Timestamp Activation

Holocene, like other network upgrades, is activated at a timestamp.  Changes to the L2 Block execution rules are applied
when the `L2 Timestamp >= activation time`.

## Dynamic EIP-1559 Parameters

### EIP-1559 Parameters in Block Header

With the Holocene upgrade, the `extraData` header field of each block must have the following format:

| Name          | Type               | Byte Offset |
| ------------- | ------------------ | ----------- |
| `version`     | `u8`               | `[0, 1)`    |
| `denominator` | `u32 (big-endian)` | `[1, 5)`    |
| `elasticity`  | `u32 (big-endian)` | `[5, 9)`    |

Additionally,

- `version` must be 0
- `denominator` must be non-zero
- there is no additional data beyond these 9 bytes

Note that `extraData` has a maximum capacity of 32 bytes (to fit in the L1 beacon-chain `extraData` data-type) and its
format may be modified/extended by future upgrades.

Note also that if the chain had Holocene genesis, the genesis block must have an above-formated `extraData` representing
the initial parameters to be used by the chain.

### EIP-1559 Parameters in `PayloadAttributesV3`

The [`PayloadAttributesV3`](https://github.com/ethereum/execution-apis/blob/cea7eeb642052f4c2e03449dc48296def4aafc24/src/engine/cancun.md#payloadattributesv3)
type is extended with an additional value, `eip1559Params`:

```rs
PayloadAttributesV3: {
    timestamp: QUANTITY
    random: DATA (32 bytes)
    suggestedFeeRecipient: DATA (20 bytes)
    withdrawals: array of WithdrawalV1
    parentBeaconBlockRoot: DATA (32 bytes)
    transactions: array of DATA
    noTxPool: bool
    gasLimit: QUANTITY or null
    eip1559Params: DATA (8 bytes) or null
}
```

#### Encoding

At and after Holocene activation, `eip1559Parameters` in `PayloadAttributeV3` must be exactly 8 bytes with the following
format:

| Name          | Type               | Byte Offset |
| ------------- | ------------------ | ----------- |
| `denominator` | `u32 (big-endian)` | `[0, 4)`    |
| `elasticity`  | `u32 (big-endian)` | `[4, 8)`    |

#### PayloadID computation

If `eip1559Params != null`, the `eip1559Params` is included in the `PayloadID` hasher directly after the `gasLimit`
field.

### Execution

#### Payload Attributes Processing

Prior to Holocene activation, `eip1559Parameters` in `PayloadAttributesV3` must be null and is otherwise considered
invalid.

At and after Holocene activation, any `ExecutionPayload` corresponding to some `PayloadAttributesV3` must contain
`extraData` formatted as the [header value](#eip-1559-parameters-in-block-header). The `denominator` and `elasticity`
values within this `extraData` must correspond to those in `eip1559Parameters`, unless both are 0.  When both are 0, the
[prior EIP-1559 constants](../exec-engine.md#1559-parameters) must be used to populate `extraData` instead.

#### Base Fee Computation

Prior to the Holocene upgrade, the EIP-1559 denominator and elasticity parameters used to compute the block base fee
were [constants](../exec-engine.md#1559-parameters).

With the Holocene upgrade, these parameters are instead determined as follows:

- if Holocene is not active in `parent_header.timestamp`, the [prior EIP-1559
  constants](../exec-engine.md#1559-parameters) constants are used. While `parent_header.extraData` is typically empty
  prior to Holocene, there are some legacy cases where it may be set arbitrarily, so it must not be assumed to be empty.
- if Holocene is active in `parent_header.timestamp`, then the parameters from `parent_header.extraData` are used.

### Rationale

Placing the EIP-1559 parameters within the L2 block header allows us to retain the purity of the function that computes
the next block's base fee from its parent block header, while still allowing them to be dynamically configured.  Dynamic
configuration is handled similarly to `gasLimit`, with the derivation pipeline providing the appropriate `SystemConfig`
contract values to the block builder via `PayloadAttributesV3` parameters.
