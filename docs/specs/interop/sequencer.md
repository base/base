# Sequencer

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Block Building](#block-building)
- [Sequencer Policy](#sequencer-policy)
  - [Safety Levels](#safety-levels)
    - [Executing Message Validation](#executing-message-validation)
    - [Transitive Dependencies](#transitive-dependencies)
- [Shared Sequencing](#shared-sequencing)
- [Security Considerations](#security-considerations)
  - [Depending on Preconfirmations](#depending-on-preconfirmations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

New validity rules are added to blocks that the sequencer must follow. If the
new rules are not followed, then the sequencer risks producing empty blocks
and reorg'ing the chain.

The term "block builder" is used interchangeably with the term "sequencer" for the purposes of this document but
they need not be the same entity in practice.

## Block Building

It is now required that a block builder fully executes a transaction and validates any executing messages
that it may produce before knowing that the transaction is valid. This adds a denial of service possibility
for the block builder. It is generally accepted that block builders can be sophisticated actors that can
build solutions for this sort of problem.

## Sequencer Policy

Sequencer policy represents the set of ways that a sequencer may act that are not constrained by consensus.
The ordering of transactions in a block is considered policy, consensus dictates that all of the transactions
in a block are valid.

OP Stack interop leverages sequencer policy to reduce synchrony assumptions between chains.
If the sequencer's view of a remote chain lags from the tip, it will not impact the overall liveness of
the network, it will only impact the liveness of inbound cross chain messages from that remote chain.
If there was a strict synchrony assumption, it could result in liveness or safety failures when any sequencer
in the cluster falls behind the tip of any remote chain.

### Safety Levels

The sequencer MAY include an executing message with any level of confirmation safety.
Including cross chain messages based on preconfirmation levels of security results
in lower latency messaging at a higher risk or an invalid block being produced.

The sequencer MAY require different levels of security depending on the source chain.
If the block containing the initiating message is considered safe, no additional trust
assumptions are assumed. The only time that additional trust assumptions are added is
when an initiating message from an unsafe block is consumed.

It is recommended to leverage the [supervisor](./supervisor.md) API to validate messages.

#### Executing Message Validation

The block builder SHOULD validate executing messages directly before including the transaction
that produced the executing message in a block. Given the async nature of many independent chains
operating in parallel, it is possible that the block builder does not have the most up to date
view of all remote chains at any given moment.

The block builder MAY require cryptographic proof of the existence of the log
that the identifier points to, if it trusts the remote canonical chain but not its RPC server.

The block builder MAY also trust a remote RPC and use the following algorithm to verify the
existence of the log. This algorithm does not check for a particular finality level of the
block that includes the initiating message.

```python
success, receipt = evm.apply_transaction(tx)

# no logs are produced for reverting transactions
if not success:
  return True

# iterate over all of the logs
for log in receipt.logs:
  if is_executing_message(log):
      id = abi.decode(log.data)
      message_hash = log.topics[1]

      # maintain a RPC client for each remote node by chainid
      eth = clients[id.chainid]

      # cannot verify messages without a client, do not include it
      if eth is None:
        return False

      # use the identifier to fetch logs
      logs = eth.get_logs(id.origin, from=id.block_number, to=id.block_number)
      filtered = filter(lambda x: x.index == id.log_index && x.address == id.origin)
      # log does not exist, do not include it
      if len(filtered) != 1:
        return False

      # ensure the contents of the log are correct
      log = encode(filtered[0])
      if message_hash != keccack256(log):
        return False

      block = eth.get_block_by_number(id.blocknumber)

      # ensure that the timestamp is correct
      if id.timestamp != block.timestamp:
        return False

return True
```

#### Transitive Dependencies

The safety of a block is inherently tied to the safety of the blocks that include initiating messages
consumed. This applies recursively, so a block builder that wants to only include safe cross chain
messages will need to recursively check that all dependencies are safe.

## Shared Sequencing

A shared sequencer can be built if the block builder is able to build the next canonical block
for multiple chains. This can enable synchronous composability where transactions are able
to execute across multiple chains at the same timestamp.

## Security Considerations

### Depending on Preconfirmations

If a local sequencer is accepting inbound cross chain transactions where the initiating message only has preconfirmation
levels of security, this means that the remote sequencer can trigger a reorg on the local chain.
