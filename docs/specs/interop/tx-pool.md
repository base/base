# Transaction pool

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Message validation](#message-validation)
- [System deposits transaction margin](#system-deposits-transaction-margin)
- [Security Considerations](#security-considerations)
  - [Mempool Denial of Service](#mempool-denial-of-service)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

The transaction-pool is part of the execution-engine,
and generally replicated by the sequencer and its surrounding infrastructure.

Not all messages may be included by the sequencer during [block building](./sequencer.md#block-building),
additional transaction-pool validation is thus required to prevent non-includable
transactions from overwhelming the network.

Transactions with cross-chain messages are subject to the same transaction-pool
validation rules as regular transactions:
nonce, balance and fee changes need to be applied to validation so the cross-chain transaction
is possible to include in a block in the first place.

However, additional validation rules are applied to demote messages that cannot be included in a block.

## Message validation

Through [static-analysis](./sequencer.md#static-analysis) as performed in block building,
the [`Identifier`] of the message is read, and used for further validation. Static analysis is
not always possible, therefore the mempool SHOULD delegate execution to another service that can
horizontally scale validation of executing messages.

The [messaging invariants](./messaging.md#messaging-invariants) should be enforced in the transaction pool,
with dependency validation adapted for guarantees of the in-flight transactions:
the executing message is not included in the chain yet,
but the validity of the message and its initiating source is tracked.

A Message with a definitively invalid message-dependency SHOULD be "demoted" from the transaction pool.

Irreversible invalidation conditions:

- The block at the initiating message source is finalized and does not pass the initiating-message checks.
- The message is expired.

The sequencer MAY choose to demote messages which are invalid but can still technically become valid:

- The block at the initiating message source is known, but another conflicting unsafe block is canonical.
- The block at the initiating message has invalidated message dependencies.

Transactions with invalid message-dependencies MUST NOT be included in block-building,
and should thus be dropped from the transaction-pool.

## System deposits transaction margin

The [Deposit context closing transaction](./derivation.md#closing-the-deposit-context) requires
a small margin of additional EVM gas to be available for system operations.

The transaction-pool should filter out L2 transactions that spend more than the
gas limit, minus the gas spent on system transactions.
This ensures that the transaction can be included in a valid L2 block,
and does not get stuck in the transaction pool.

A notion of an "effective gas limit", that subtracts 100,000 gas from the regular gas limit,
should be maintained in the transaction pool.
This leaves sufficient gas for the L1 attributes transaction (under 45,000 gas),
the new deposit-context closing transaction (under 36,000 gas), and margin for error / change.

## Security Considerations

### Mempool Denial of Service

Since the validation of the executing message relies on a remote RPC request, this introduces a denial of
service attack vector. The cost of network access is magnitudes larger than in memory validity checks.
The mempool SHOULD perform low-cost checks before any sort of network access is performed.
The results of the check SHOULD be cached such that another request does not need to be performed
when building the block although consistency is not guaranteed.
