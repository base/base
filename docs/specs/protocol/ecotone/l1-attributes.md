# Ecotone L1 Attributes

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [L1 Attributes Predeployed Contract](#l1-attributes-predeployed-contract)
  - [Ecotone L1Block upgrade](#ecotone-l1block-upgrade)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

On the Ecotone activation block, and if Ecotone is not activated at Genesis,
the L1 Attributes Transaction includes a call to `setL1BlockValues()`
because the L1 Attributes transaction precedes the [Ecotone Upgrade Transactions][ecotone-upgrade-txs],
meaning that `setL1BlockValuesEcotone` is not guaranteed to exist yet.

Every subsequent L1 Attributes transaction should include a call to the `setL1BlockValuesEcotone()` function.
The input args are no longer ABI encoded function parameters,
but are instead packed into 5 32-byte aligned segments (starting after the function selector).
Each unsigned integer argument is encoded as big-endian using a number of bytes corresponding to the underlying type.
The overall calldata layout is as follows:

[ecotone-upgrade-txs]: derivation.md#network-upgrade-automation-transactions

| Input arg         | Type    | Calldata bytes | Segment |
| ----------------- | ------- | -------------- | ------- |
| {0x440a5e20}      |         | 0-3            | n/a     |
| baseFeeScalar     | uint32  | 4-7            | 1       |
| blobBaseFeeScalar | uint32  | 8-11           |         |
| sequenceNumber    | uint64  | 12-19          |         |
| l1BlockTimestamp  | uint64  | 20-27          |         |
| l1BlockNumber     | uint64  | 28-35          |         |
| basefee           | uint256 | 36-67          | 2       |
| blobBaseFee       | uint256 | 68-99          | 3       |
| l1BlockHash       | bytes32 | 100-131        | 4       |
| batcherHash       | bytes32 | 132-163        | 5       |

Total calldata length MUST be exactly 164 bytes, implying the sixth and final segment is only
partially filled. This helps to slow database growth as every L2 block includes a L1 Attributes
deposit transaction.

In the first L2 block after the Ecotone activation block, the Ecotone L1 attributes are first used.

The pre-Ecotone values are migrated over 1:1.
Blocks after the Ecotone activation block contain all pre-Ecotone values 1:1,
and also set the following new attributes:

- The `baseFeeScalar` is set to the pre-Ecotone `scalar` value.
- The `blobBaseFeeScalar` is set to `0`.
- The pre-Ecotone `overhead` attribute is dropped.
- The `blobBaseFee` is set to the L1 blob base fee of the L1 origin block.
  Or `1` if the L1 block does not support blobs.
  The `1` value is derived from the EIP-4844 `MIN_BLOB_GASPRICE`.

## L1 Attributes Predeployed Contract

[sys-config]: ../system-config.md

The L1 Attributes predeploy stores the following values:

- L1 block attributes:
  - `number` (`uint64`)
  - `timestamp` (`uint64`)
  - `basefee` (`uint256`)
  - `hash` (`bytes32`)
  - `blobBaseFee` (`uint256`)
- `sequenceNumber` (`uint64`): This equals the L2 block number relative to the start of the epoch,
  i.e. the L2 block distance to the L2 block height that the L1 attributes last changed,
  and reset to 0 at the start of a new epoch.
- System configurables tied to the L1 block, see [System configuration specification][sys-config]:
  - `batcherHash` (`bytes32`): A versioned commitment to the batch-submitter(s) currently operating.
  - `baseFeeScalar` (`uint32`): system configurable to scale the `basefee` in the Ecotone l1 cost computation
  - `blobBasefeeScalar` (`uint32`): system configurable to scale the `blobBaseFee` in the Ecotone l1 cost computation

The `overhead` and `scalar` values can continue to be accessed after the Ecotone activation block,
but no longer have any effect on system operation. These fields were also known as the `l1FeeOverhead`
and the `l1FeeScalar`.

After running `pnpm build` in the `packages/contracts-bedrock` directory, the bytecode to add to
the genesis file will be located in the `deployedBytecode` field of the build artifacts file at
`/packages/contracts-bedrock/forge-artifacts/L1Block.sol/L1Block.json`.

### Ecotone L1Block upgrade

The L1 Attributes Predeployed contract, `L1Block.sol`, is upgraded as part of the Ecotone upgrade.
The version is incremented to `1.2.0`, one new storage slot is introduced, and one existing slot
begins to store additional data:

- `blobBaseFee` (`uint256`): The L1 blob base fee.
- `blobBaseFeeScalar` (`uint32`): The scalar value applied to the L1 blob base fee portion of the L1 cost.
- `baseFeeScalar` (`uint32`): The scalar value applied to the L1 base fee portion of the L1 cost.

The function called by the L1 attributes transaction depends on the network upgrade:

- Before the Ecotone activation:
  - `setL1BlockValues` is called, following the pre-Ecotone L1 attributes rules.
- At the Ecotone activation block:
  - `setL1BlockValues` function MUST be called, except if activated at genesis.
    The contract is upgraded later in this block, to support `setL1BlockValuesEcotone`.
- After the Ecotone activation:
  - `setL1BlockValues` function is deprecated and MUST never be called.
  - `setL1BlockValuesEcotone` MUST be called with the new Ecotone attributes.

`setL1BlockValuesEcotone` uses a tightly packed encoding for its parameters, which is described in
[L1 Attributes Deposited Transaction Calldata](../deposits.md#l1-attributes-deposited-transaction-calldata).
