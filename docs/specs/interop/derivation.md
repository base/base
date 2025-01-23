# Derivation

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Invariants](#invariants)
- [L1 Attributes Transaction](#l1-attributes-transaction)
  - [Deposit Context](#deposit-context)
    - [Opening the deposit context](#opening-the-deposit-context)
    - [Closing the deposit context](#closing-the-deposit-context)
      - [Deposits-complete Source-hash](#deposits-complete-source-hash)
- [Replacing Invalid Blocks](#replacing-invalid-blocks)
  - [Optimistic Block Deposited Transaction](#optimistic-block-deposited-transaction)
- [Expiry Window](#expiry-window)
- [Security Considerations](#security-considerations)
  - [Gas Considerations](#gas-considerations)
  - [Depositing an Executing Message](#depositing-an-executing-message)
  - [Reliance on History](#reliance-on-history)
  - [Expiry Window](#expiry-window-1)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

New derivation rules are added to guaranteee integrity of cross chain messages.
The fork choice rule is updated to fork out unsafe blocks that contain invalid
executing messages.

## Invariants

- An executing message MUST have a corresponding initiating message
- The initiating message referenced in an executing message MUST come from a chain in its dependency set
- A block MUST be considered invalid if it is built with any invalid executing messages
- The timestamp of the identifier MUST be greater than or equal to the interop network upgrade timestamp
- The timestamp of the identifier MUST be less than or equal to the timestamp of the block that includes it
- The timestamp of the identifier MUST be greater than timestamp of the block that includes it minus the expiry window

L2 blocks that produce invalid executing messages MUST not be allowed to be considered safe.
They MAY optimistically exist as unsafe blocks for some period of time. An L2 block that is invalidated
because it includes invalid executing messages MUST be replaced by a deposits only block at the same
block height. This guarantees progression of the chain, ensuring that an infinite loop of processing
the same block in the proof system is not possible.

## L1 Attributes Transaction

The L1 attributes transaction is updated to a new entrypoint on the `L1Block` contract.

### Deposit Context

Derivation is extended to create **deposit contexts**, which signify the execution of a depositing transaction.
A deposit context is scoped to a single block, opening with the first deposited transaction and closing after
the execution of the final deposited transaction. As such, there is exactly one deposit context per block.
The deposit context exists to give legibility within the EVM that execution is happening in the context of
a deposit transaction. See [`isDeposit()`](./predeploys.md#isdeposit) for more information.

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

## Replacing Invalid Blocks

When the [cross chain dependency resolution](./messaging.md#resolving-cross-chain-safety) determines
that a block contains an [invalid message](./messaging.md#invalid-messages), the block is replaced
by a block with the same inputs, except for the transactions included. The transactions from the
original block are trimmed to include only deposit transactions plus an
[optimistic block info deposit transaction](#optimistic-block-deposited-transaction), which is appended
to the trimmed transaction list.

### Optimistic Block Deposited Transaction

An [L1 attributes deposited transaction](../protocol/deposits.md#l1-attributes-deposited-transaction)
is a deposit transaction sent to the zero address.

This transaction MUST have the following values:

1. `from` is `0xdeaddeaddeaddeaddeaddeaddeaddeaddead0002` (the address of the
   [L1 Attributes depositor account](../protocol/deposits.md#l1-attributes-depositor-account))
2. `to` is `0x0000000000000000000000000000000000000000` (the zero address as no EVM code execution is expected).
3. `mint` is `0`
4. `value` is `0`
5. `gasLimit` is set `36000` gas, to cover intrinsic costs, processing costs, and margin for change.
6. `isSystemTx` is set to `false`.
7. `data` is the preimage of the [L2 output root](../glossary.md#l2-output-root-proposals)
   of the replaced block. i.e. `version_byte || payload` without applying the `keccak256` hashing.

This system-initiated transaction for L1 attributes is not charged any ETH for its allocated
`gasLimit`, as it is considered part of state-transition processing.

## Expiry Window

The expiry window is the time period after which an initiating message is no longer considered valid.

| Constant | Value |
| -------- | ----- |
| `EXPIRY_WINDOW` | `TODO` |

## Security Considerations

### Gas Considerations

There must be sufficient gas available in the block to destroy deposit context. Depending on the
chain configuration, there is no guarantee that the amount of guaranteed gas plus the amount used
by the system transactions is less than the L2 block gas limit. It is important that the chain operator
maintains a ~9 million gas buffer between the guaranteed gas limit and the L2 gas limit.

### Depositing an Executing Message

Deposit transactions (force inclusion transactions) give censorship resistance to layer two networks.
It is possible to deposit an invalid executing message, forcing the sequencer to reorg. It would
be fairly cheap to continuously deposit invalid executing messages through L1 and cause L2 liveness
instability. A future upgrade will enable deposits to trigger executing messages.

### Reliance on History

When fully executing historical blocks, a dependency on historical receipts from remote chains is present.
[EIP-4444][eip-4444] will eventually provide a solution for making historical receipts available without
needing to execute increasingly long chain histories.

[eip-4444]: https://eips.ethereum.org/EIPS/eip-4444

### Expiry Window

The expiry window ensures that the proof can execute in a reasonable amount of time.
There is currently no way to prove old history with a sublinear proof size. The proof
program needs to walk back and reexecute to reproduce the consumed logs. This means
that very old logs are more expensive to prove.
