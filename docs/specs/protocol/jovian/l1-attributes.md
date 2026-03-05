# L1 Block Attributes

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The L1 block attributes transaction is updated to include the DA footprint gas scalar.

| Input arg         | Type    | Calldata bytes | Segment |
| ----------------- | ------- | -------------- | ------- |
| {0x3db6be2b}      |         | 0-3            | n/a     |
| baseFeeScalar     | uint32  | 4-7            | 1       |
| blobBaseFeeScalar | uint32  | 8-11           |         |
| sequenceNumber    | uint64  | 12-19          |         |
| l1BlockTimestamp  | uint64  | 20-27          |         |
| l1BlockNumber     | uint64  | 28-35          |         |
| basefee           | uint256 | 36-67          | 2       |
| blobBaseFee       | uint256 | 68-99          | 3       |
| l1BlockHash       | bytes32 | 100-131        | 4       |
| batcherHash       | bytes32 | 132-163        | 5       |
| operatorFeeScalar   | uint32  | 164-167      | 6       |
| operatorFeeConstant | uint64  | 168-175      |         |
| daFootprintGasScalar | uint16  | 176-177     |         |

Note that the first input argument, in the same pattern as previous versions of the L1 attributes transaction,
is the function selector: the first four bytes of `keccak256("setL1BlockValuesJovian()")`.

In the activation block, there are two possibilities:
- If Jovian is active at genesis, there are no transactions in the activation block
and therefore no L1 Block Attributes transaction to consider.
- If Jovian activates after genesis [`setL1BlockValuesIsthmus()`](../isthmus/l1-attributes.md) method must be used.
 This is because the L1 Block contract will not yet have been upgraded.

In each subsequent L2 block, the `setL1BlockValuesJovian()` method must be used.

When using this method, the pre-Jovian values are migrated over 1:1
and the transaction also sets `daFootprintGasScalar` to the
value from the [`SystemConfig`](./system-config.md). If that value is `0`, then a default of `400` is set.
