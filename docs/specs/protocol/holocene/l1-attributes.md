# L1 Block Attributes

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The L1 block attributes transaction is updated to include the EIP-1559 parameters.

| Input arg         | Type    | Calldata bytes | Segment |
| ----------------- | ------- | -------------- | ------- |
| {0xd1fbe15b}      |         | 0-3            | n/a     |
| baseFeeScalar     | uint32  | 4-7            | 1       |
| blobBaseFeeScalar | uint32  | 8-11           |         |
| sequenceNumber    | uint64  | 12-19          |         |
| l1BlockTimestamp  | uint64  | 20-27          |         |
| l1BlockNumber     | uint64  | 28-35          |         |
| basefee           | uint256 | 36-67          | 2       |
| blobBaseFee       | uint256 | 68-99          | 3       |
| l1BlockHash       | bytes32 | 100-131        | 4       |
| batcherHash       | bytes32 | 132-163        | 5       |
| eip1559Denominator   | uint64  | 164-171        | 6       |
| eip1559Elasticity    | uint64  | 172-179        |         |
