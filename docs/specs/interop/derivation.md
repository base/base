# Derivation

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
  - [Deposit Context](#deposit-context)
    - [Opening the deposit context](#opening-the-deposit-context)
    - [Closing the deposit context](#closing-the-deposit-context)
      - [Deposits-complete Source-hash](#deposits-complete-source-hash)
- [Security Considerations](#security-considerations)
  - [Gas Considerations](#gas-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

### Deposit Context

Derivation is extended to create **deposit contexts**, which signifies the execution of a depositing transaction.
A deposit context is scoped to a single block, commencing with the execution of the first deposited transaction
and concluding immediately after the execution of the final deposited transaction within that block.
As such, there is exactly one deposit context per block.

The order of deposit transactions occurs as follows:

1. L1 attributes transaction, [opening the deposit context](#opening-the-deposit-context).
2. User deposits (if any).
3. L1 attributes transaction, [closing the deposit context](#closing-the-deposit-context).
4. During upgrades, additional deposits, inserted by derivation, may follow after the above.
   See [upgrade specification](./upgrade.md).

The L1 attributes operations wrap user deposits,
such that `isDeposit = true` occurs during the first L1 attributes transaction
and `isDeposit = false` occurs immediately after the last user deposit,
if any exists, or after the first L1 attributes transaction if there are no user deposits.

#### Opening the deposit context

A new `L1Block` predeploy function is introduced to set the L1 attributes: `setL1BlockValuesIsthmus()`.

The block-attributes contents are unchanged from the previous fork.
See [Ecotone L1 attributes specifications](../protocol/ecotone/l1-attributes.md),
and [Holocene specifications](../protocol/holocene/overview.md).

In addition to the setting L1 block attributes, the `setL1BlockValuesIsthmus` function
now sets `isDeposit = true` on the `L1Block` predeploy contract.

This instantiates a deposit context for the current block.

#### Closing the deposit context

A new `L1Block` predeploy function is introduced to close the deposit context: `depositsComplete()`.

This sets `isDeposit = false` in the `L1Block` predeploy contract.

This function is called by a new L1 block system transaction.
This transaction MUST have the following values:

1. `from` is `0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001`, the address of the
   [L1 Attributes depositor account](../protocol/deposits.md#l1-attributes-depositor-account).
2. `to` is `0x4200000000000000000000000000000000000015`, the address of the
   [L1 attributes predeployed contract](../protocol/deposits.md#l1-attributes-predeployed-contract).
3. `mint` is `0`.
4. `value` is `0`.
5. `gasLimit` is set `36000` gas, to cover intrinsic costs, processing costs, and margin for change.
6. `isSystemTx` is `false`.
7. `data` is set to `0xe32d20bb`, the 4-byte selector of `depositsComplete()`.
 This closes the existing deposit context.
8. `sourceHash` is computed with a new deposit source-hash domain, see below.

##### Deposits-complete Source-hash

The source hash is [computed](../protocol/deposits.md#source-hash-computation) alike to that
of the "L1 attributes deposited" deposit that opens the deposit context.
The one difference is the source-hash domain: `3` (instead of `1`).

The source-hash is thus computed as:
`keccak256(bytes32(uint256(3)), keccak256(l1BlockHash, bytes32(uint256(seqNumber))))`.

Where `l1BlockHash` refers to the L1 block hash of which the info attributes are deposited.
And `seqNumber = l2BlockNum - l2EpochStartBlockNum`,
where `l2BlockNum` is the L2 block number of the inclusion of the deposit tx in L2,
and `l2EpochStartBlockNum` is the L2 block number of the first L2 block in the epoch.

## Security Considerations

### Gas Considerations

There must be sufficient gas available in the block to destroy deposit context.
There's no guarantee on the minimum gas available for the second L1 attributes transaction as the block
may be filled by the other deposit transactions. As a consequence, a deposit context may spill into multiple blocks.

This will be fixed in the future.
