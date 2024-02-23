# Diff Based L1Attributes Transaction

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Implementation](#implementation)
  - [Ecotone L1Attributes Example](#ecotone-l1attributes-example)
  - [Optimization](#optimization)
- [Node Implementation](#node-implementation)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

The `L1Attributes` transaction is the first transaction included in every L2 block.
It can be thought of as an extension to the L2 block header that is 100% backwards
compatible with L1 Ethereum by not changing the structure of the block header itself.
A major problem with the `L1Attributes` transaction is that it constantly submits the
same information over and over again. This adds chain history that must be preserved
forever. Ideally, the `L1Attributes` transaction only includes diffs to reduce the
growth of the chain history.

## Implementation

The `L1Attributes` transaction already commits to a schema that is ABI encoded.
There is no concept of an `Optional` type in ABI encoding, so instead we can
utilize a custom data structure to indicate the presence of a field in the schema.
A leading bitfield can be used to use presence. Each bit indicates a specific item
in the schema where `1` means it is present and `0` means it is not present. Each
item MUST have at least a fixed size portion so that the decoder knows how far
forward to seek after a read when the item is present.

### Ecotone L1Attributes Example

The following fields are required as part of the `L1Attributes` transaction after
the Ecotone network upgrade.

| Field             | Size (bytes) | Bitfield Representation  |
| ----------------- | ------------ | ------------------------ |
| baseFeeScalar     | 4            | 0b1000\_0000\_0000\_0000 |
| blobBaseFeeScalar | 4            | 0b0100\_0000\_0000\_0000 |
| sequenceNumber    | 8            | 0b0010\_0000\_0000\_0000 |
| l1BlockTimestamp  | 8            | 0b0001\_0000\_0000\_0000 |
| l1BlockNumber     | 8            | 0b0000\_1000\_0000\_0000 |
| basefee           | 32           | 0b0000\_0100\_0000\_0000 |
| blobBaseFee       | 32           | 0b0000\_0010\_0000\_0000 |
| l1BlockHash       | 32           | 0b0000\_0001\_0000\_0000 |
| batcherHash       | 32           | 0b0000\_0000\_1000\_0000 |

This is a total of 9 fields, so 9 bits are required to represent all of them.
It cannot fit into a single byte, so 2 bytes should be used to represent the bitfield.
Following the bitfield is the data tightly packed together.
When the value is set to `1` in the location that corresponds to the field, then
it indicates that the value is present and the the `size` number of bytes should
be read from the `calldata` and the offset of the next read from the calldata is
incremented by the `size`. If the value in the bitfield is a `0`, then the data
is not present and any reading/seeking is skipped.

### Optimization

The above example can be optimized by grouping the attributes that always change together
into a single bit. This would be the `l1BlockTimestamp`, the `l1BlockNumber` and `l1BlockHash`.
In the case of a missed L1 slot or L1 reorg, it is possible to have the `l1BlockNumber` not change but
the others may change. This case doesn't happen often and would result in some repeat data being
posted.

It is not safe to omit the `sequenceNumber` due to the fact that it always increases because we need
to guarantee that L1 Attributes Transactions have unique transaction hashes. In this way it acts as a nonce.

## Node Implementation

Previously, the node could read the calldata from the latest `L1Attributes` transaction
to be able to populate it's internal view of the `SystemConfig`. This will not be possible
with a diff based `L1Attributes` transaction implementation, but the problem can be solved
easily with a batch RPC call that reads each value directly from state using `eth_call`.
