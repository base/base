# Transaction pool

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Transaction validation](#transaction-validation)
- [System deposits transaction margin](#system-deposits-transaction-margin)
- [Security Considerations](#security-considerations)
  - [Mempool Denial of Service](#mempool-denial-of-service)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The transaction pool (also known as the mempool) is where pending user transactions accumulate
before being included in a block. When a user submits a transaction to a node, it is validated
and inserted into the node's transaction pool before being broadcasted via p2p network to other nodes.

Since there is little cost to sending invalid transactions over the p2p network maliciously,
Ethereum opts to ensure that the transactions can be validated as cheaply as possible. This validation
consists of a nonce and balance (fee payment) check, which can be done with solely state lookups and
the chain tips block header. Features that make this validation more costly have been rejected from L1
Ethereum due to the desire to ensure [commodity hardware](https://hackmd.io/@kevaundray/S1hUQuV4Jx) can
easily participate in consensus.

Layer twos can make more aggressive trade-offs and raise the minimum hardware requirements of the network.
However, with interop, full EVM execution of the transaction is required to fully validate it.

## Transaction validation

In addition to the nonce and balance checks, the [messaging invariants](./messaging.md#messaging-invariants)
SHOULD be enforced before entry into the transaction pool.

After each new block, each transaction in the transaction pool is checked for validity again.
A transaction with a definitively invalid message-dependency SHOULD be "demoted" from the transaction pool.

It is possible that a transaction deemed valid becomes invalid or vice versa. The sequencer MAY choose
to demote messages which are invalid but can still technically become valid.

Transactions with invalid message-dependencies MUST NOT be included in block-building,
and should thus be dropped from the transaction-pool.

## System deposits transaction margin

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
The results of the check SHOULD be cached, such that another request does not need to be performed
when building the block, although consistency is not guaranteed.
