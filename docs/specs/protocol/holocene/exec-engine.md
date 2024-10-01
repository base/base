# L2 Execution Engine

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Timestamp Activation](#timestamp-activation)
- [`L2ToL1MessagePasser` Storage Root in Header](#l2tol1messagepasser-storage-root-in-header)
  - [Header Validity Rules](#header-validity-rules)
  - [Header Withdrawals Root](#header-withdrawals-root)
    - [Rationale](#rationale)
    - [Forwards Compatibility Considerations](#forwards-compatibility-considerations)
    - [Client Implementation Considerations](#client-implementation-considerations)
- [Extended `PayloadAttributesV3`](#extended-payloadattributesv3)
  - [`eip1559Params` encoding](#eip1559params-encoding)
  - [Execution](#execution)
  - [Rationale](#rationale-1)
- [`eip1559Params` in Header](#eip1559params-in-header)
  - [Header Validity Rules](#header-validity-rules-1)
  - [Encoding](#encoding)
  - [Rationale](#rationale-2)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Timestamp Activation

Holocene, like other network upgrades, is activated at a timestamp.
Changes to the L2 Block execution rules are applied when the `L2 Timestamp >= activation time`.

## `L2ToL1MessagePasser` Storage Root in Header

After Holocene's activation, the L2 block header's `withdrawalsRoot` field will consist of the 32-byte
[`L2ToL1MessagePasser`][l2-to-l1-mp] account storage root _after_ the block has been executed, and _after_ the
insertions and deletions have been applied to the trie. In other words, the storage root should be the same root
that is returned by `eth_getProof` at the given block number.

### Header Validity Rules

Prior to holocene activation, the L2 block header's `withdrawalsRoot` field must be:

- `nil` if Canyon has not been activated.
- `keccak256(rlp(empty_string_code))` if Canyon has been activated.

After Holocene activation, an L2 block header's `withdrawalsRoot` field is valid iff:

1. It is exactly 32 bytes in length.
1. The [`L2ToL1MessagePasser`][l2-to-l1-mp] account storage root, as committed to in the `storageRoot` within the block
   header, is equal to the header's `withdrawalsRoot` field.

### Header Withdrawals Root

| Byte offset | Description                                               |
| ----------- | --------------------------------------------------------- |
| `[0, 32)`   | [`L2ToL1MessagePasser`][l2-to-l1-mp] account storage root |

#### Rationale

Currently, to generate [L2 output roots][output-root] for historical blocks, an archival node is required. This directly
places a burden on users of the system in a post-fault-proofs world, where:

1. A proposer must have an archive node to propose an output root at the safe head.
1. A user that is proving their withdrawal must have an archive node to verify that the output root they are proving
   their withdrawal against is indeed valid and included within the safe chain.

Placing the [`L2ToL1MessagePasser`][l2-to-l1-mp] account storage root in the `withdrawalsRoot` field alleviates this burden
for users and protocol participants alike, allowing them to propose and verify other proposals with lower operating costs.

#### Forwards Compatibility Considerations

As it stands, the `withdrawalsRoot` field is unused within the OP Stack's header consensus format, and will never be
used for other reasons that are currently planned. Setting this value to the account storage root of the withdrawal
directly fits with the OP Stack, and makes use of the existing field in the L1 header consensus format.

#### Client Implementation Considerations

Varous EL clients store historical state of accounts differently. If, as a contrived case, an OP Stack chain did not have
an outbound withdrawal for a long period of time, the node may not have access to the account storage root of the
[`L2ToL1MessagePasser`][l2-to-l1-mp]. In this case, the client would be unable to keep consensus. However, most modern
clients are able to at the very least reconstruct the account storage root at a given block on the fly if it does not
directly store this information.

## Extended `PayloadAttributesV3`

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

### `eip1559Params` encoding

| Name          | Type               | Byte Offset |
| ------------- | ------------------ | ----------- |
| `denominator` | `u32 (big-endian)` | `[0, 4)`    |
| `elasticity`  | `u32 (big-endian)` | `[4, 8)`    |

### Execution

During execution, the EIP-1559 parameters used to calculate the next block base fee should come from the
`PayloadAttributesV3` type rather than the previous protocol constants, if it is non-null.

- If, before Holocene activation, `eip1559Parameters` is non-zero, the attributes are to be considered invalid by the
  engine.
- After Holocene activation:
  - if `eip1559Params` is zero, the [canyon base fee parameter constants](../exec-engine.md#1559-parameters) are
    used.
  - if `eip1559Params` are non-null, the values from the attributes are used.

### Rationale

This type is made available in the payload attributes to allow the block builder to dynamically control the EIP-1559
parameters of the chain. As described in the [derivation - AttributesBuilder](./derivation.md#attributes-builder)
section, the derivation pipeline must populate this field from the `SystemConfig` during payload building, similar to
how it must reference the `SystemConfig` for the `gasLimit` field.

## `eip1559Params` in Header

Upon Holocene activation, the L2 block header's `nonce` field will consist of the 8-byte `eip1559Params` value.

### Header Validity Rules

Prior to Holocene activation, the L2 block header's `nonce` field is valid iff it is equal to `u64(0)`.

After Holocene activation, The L2 block header's `nonce` field is valid iff it is non-zero.

### Encoding

The encoding of the `eip1559Params` value is described in [`eip1559Params` encoding](#eip1559params-encoding).

### Rationale

By chosing to put the `eip1559Params` in the `PayloadAttributes` rather than in the L1 block info transaction,
the EIP-1559 parameters for the chain are not available within history. This would place a burden on performing
historical execution, as L1 would have to be consulted for fetching the values from the `SystemConfig` contract.
Instead, we re-use an unused field in the L1 block header as to make these parameters available, retaining the
purity of the function that computes the next block's base fee from the chain configuration, parent block header,
and next block timestamp.

[l2-to-l1-mp]: ../../protocol/predeploys.md#L2ToL1MessagePasser
[output-root]: ../../glossary.md#l2-output-root
