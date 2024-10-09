# L2 Execution Engine

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Timestamp Activation](#timestamp-activation)
- [Dynamic EIP-1559 Parameters](#dynamic-eip-1559-parameters)
  - [Extended `PayloadAttributesV3`](#extended-payloadattributesv3)
    - [PayloadID computation](#payloadid-computation)
    - [`eip1559Params` encoding](#eip1559params-encoding)
  - [Execution](#execution)
  - [Rationale](#rationale)
  - [`eip1559Params` in Header](#eip1559params-in-header)
    - [Header Validity Rules](#header-validity-rules)
    - [Encoding](#encoding)
    - [Rationale](#rationale-1)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The EIP-1559 parameters are encoded in the block header's `nonce` field and can be
configured dynamically through the `SystemConfig`.

## Timestamp Activation

Holocene, like other network upgrades, is activated at a timestamp.
Changes to the L2 Block execution rules are applied when the `L2 Timestamp >= activation time`.

## Dynamic EIP-1559 Parameters

### Extended `PayloadAttributesV3`

The [`PayloadAttributesV3`](https://github.com/ethereum/execution-apis/blob/cea7eeb642052f4c2e03449dc48296def4aafc24/src/engine/cancun.md#payloadattributesv3)
type is extended to:

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
    eip1559Params: DATA (8 bytes)
}
```

#### PayloadID computation

If `eip1559Params != null`, the `eip1559Params` is included in the `PayloadID` hasher directly after the `gasLimit`
field.

#### `eip1559Params` encoding

| Name          | Type               | Byte Offset |
| ------------- | ------------------ | ----------- |
| `denominator` | `u32 (big-endian)` | `[0, 4)`    |
| `elasticity`  | `u32 (big-endian)` | `[4, 8)`    |

### Execution

During execution, the EIP-1559 parameters used to calculate the next block base fee should come from the
parent header's `nonce` field rather than the previous protocol constants, if it is non-zero.

- If, before Holocene activation, `eip1559Parameters` in the `PayloadAttributesV3` is non-null, the attributes are to
  be considered invalid by the engine.
- At and after Holocene activation:
  - if `eip1559Params` in the `PayloadAttributesV3` is null, the attributes are to be considered invalid by the
    engine.
  - if `eip1559Params`' `denominator` is `0`, the attributes are to be considered invalid by the engine.
  - if `parent_header.nonce` is zero, the [canyon base fee parameter constants](../exec-engine.md#1559-parameters) are
    used for the block's base fee parameters.
  - if `parent_header.nonce` is non-zero, the EIP-1559 parameters encoded within the parent header's `nonce` field are
    used for the block's base fee parameters.

### Rationale

This type is made available in the payload attributes to allow the block builder to dynamically control the EIP-1559
parameters of the chain. As described in the [derivation - AttributesBuilder](./derivation.md#attributes-builder)
section, the derivation pipeline must populate this field from the `SystemConfig` during payload building, similar to
how it must reference the `SystemConfig` for the `gasLimit` field.

### `eip1559Params` in Header

Upon Holocene activation, the L2 block header's `nonce` field will consist of the 8-byte `eip1559Params` value from
the `PayloadAttributesV3`, or the canyon EIP-1559 constants if `eip1559Params` is equal to zero.

#### Header Validity Rules

Prior to Holocene activation, the L2 block header's `nonce` field is valid iff it is equal to `u64(0)`.

At and after Holocene activation, The L2 block header's `nonce` field is valid iff it is non-zero.

#### Encoding

The encoding of the `eip1559Params` value is described in [`eip1559Params` encoding](#eip1559params-encoding).

#### Rationale

By chosing to put the `eip1559Params` in the `PayloadAttributes` rather than in the L1 block info transaction,
the EIP-1559 parameters for the chain are not available within history. This would place a burden on performing
historical execution, as L1 would have to be consulted for fetching the values from the `SystemConfig` contract.
Instead, we re-use an unused field in the L1 block header as to make these parameters available, retaining the
purity of the function that computes the next block's base fee from the chain configuration, parent block header,
and next block timestamp.

[l2-to-l1-mp]: ../../protocol/predeploys.md#L2ToL1MessagePasser
[output-root]: ../../glossary.md#l2-output-root
