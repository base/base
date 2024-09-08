# Derivation

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
  - [Deposit Context](#deposit-context)
- [Security Considerations](#security-considerations)
  - [Gas Considerations](#gas-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

### Deposit Context

Derivation is extended to create **deposit contexts**, which signifies the execution of a depositing transaction.
A deposit context is scoped to a single block, commencing with the execution of the first deposited transaction
and concluding immediately after the execution of the final deposited transaction within that block.
As such, there is exactly one deposit context per block.

A deposit context is created by two operations:

- An L1 attributes transaction that sets `isDeposit = true` on the `L1Block` contract.
This instantiates a deposit context for the current block.
- An L1 attributes transaction that sets `isDeposit = false`. This destroys the existing deposit context.

These two operations wrap user deposits, such that `isDeposit = true` occurs during the first L1 attributes
transaction and `isDeposit = false` occurs immediately after the last user deposit,
if any exists, or after the first L1 attributes transaction if there are no user deposits.

The order of deposit transactions occurs as follows:

1. L1 attributes transaction calling [`setL1BlockValuesIsthmus()`](../protocol/ecotone/l1-attributes.md).
1. User deposits
1. L1 attributes transaction calling [`depositsComplete()`](../protocol/ecotone/l1-attributes.md)

## Security Considerations

### Gas Considerations

There must be sufficient gas available in the block to destroy deposit context.
There's no guarantee on the minimum gas available for the second L1 attributes transaction as the block
may be filled by the other deposit transactions. As a consequence, a deposit context may spill into multiple blocks.

This will be fixed in the future.
