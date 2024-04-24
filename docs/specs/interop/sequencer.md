# Sequencer

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Sequencer Policy](#sequencer-policy)
- [Block Building](#block-building)
  - [Static analysis](#static-analysis)
  - [Dependency confirmations](#dependency-confirmations)
    - [Pre-confirmations](#pre-confirmations)
      - [Streaming pre-confirmations: "shreds"](#streaming-pre-confirmations-shreds)
    - [Direct-dependency confirmation](#direct-dependency-confirmation)
    - [Transitive-dependency confirmation](#transitive-dependency-confirmation)
- [Sponsorship](#sponsorship)
- [Security Considerations](#security-considerations)
  - [Cross Chain Message Latency](#cross-chain-message-latency)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Sequencer Policy

Sequencer Policy is the process of optimistically enacting rules outside of consensus
(the state-transition function in this context), and the choices can then be asynchronously validated
by [verifiers](./verifier.md) and [the fault-proof](./fault_proof.md).

In the context of superchain interopability, sequencer policy is utilized to enable cross-chain message relay
without adding additional state-transition complexity or cross-chain synchronicity to the protocol.

## Block Building

The goal is to present information in a way where it is as efficient as possible for the block builder to only include
executing messages that have a corresponding initiating message. It is not possible to enforce the ability to
statically analyze a transaction, so execution MAY be required to determine the information required to include
executing messages.

### Static analysis

Note that static analysis is not always reliable, but it is far faster than having to perform
execution to get the data required to validate an executing message.

The block builder SHOULD use static analysis when possible on executing messages to determine
the dependency of the message.

When a transaction has a top level [to][tx-to] field that is equal to the `CrossL2Inbox`
and the 4-byte selector in the calldata matches the entrypoint interface,
the block builder should use the chain-ID that is encoded in the `Identifier` to determine which chain includes
the initiating transaction.

### Dependency confirmations

The sequencer MAY include an executing message in a block with any desired level
of confirmation safety around the dependency of the message.

Confirmation levels:

- pre-confirmation: through direct signaling by the block builder of the initiating message.
- direct-dependency confirmation: verify the inclusion of the initiating message, but not the transitive dependencies.
- transitive-dependency confirmation: verify the message and all transitive dependencies
  are included in canonical blocks within the specified safety-view (unsafe/safe/finalized).

When operating at lower-safety confirmation levels, the block builder SHOULD re-validate included
executing messages at an increased safety level, before the block is published.

#### Pre-confirmations

The block builder can include executing messages that have corresponding initiating messages
that only have pre-confirmation levels of security if they trust the block builder that initiates.

Using an allowlist and identity turns sequencing into an integrated game which increases the ability for
block builders to trust each other. Better pre-confirmation technology will help to scale the block builder
set to untrusted actors.

Without pre-confirmations, the block builder cannot include messages of other concurrent block building work.

Pre-confirmations MAY be invalidated, and assumptions around message-validity SHOULD be treated with care.

##### Streaming pre-confirmations: "shreds"

In the context of low-latency block building, each pre-confirmation may be communicated in the form of a "shred":
the most minimal atomic state-change that can be communicated between actors in the system.

In the context of cross-chain block-building, shreds may be used to communicate initiated cross-chain messages,
to validate executing messages against.

Shreds may be streamed, and potentially signal rewinds in case of invalidated dependencies.

This block builder feature is in ongoing research,
and may be developed and experimented with between block builders without further protocol rules changes.

#### Direct-dependency confirmation

By verifying the direct dependency the block-builder does not have to implement pre-confirmations,
and can rely on its direct view of the remote chain.

The block builder MAY require cryptographic proof of the existence of the log
that the identifier points to, if it trusts the remote canonical chain but not its RPC server.

The block builder MAY also trust a remote RPC and use the following algorithm
to verify the existence of the log.

The following pseudocode represents how to check existence of a log based on an `Identifier`.
If the value `True` is returned, then it is safe to include the transaction.

```python
success, receipt = evm.apply_transaction(tx)

if not success:
  return True

for log in receipt.logs:
  if is_executing_message(log):
      id, message = abi.decode(log.data)

      # assumes there is a client for each chain in the dependency set
      eth = clients[id.chainid]

      if eth is None:
        return False

      logs = eth.getLogs(id.origin, from=id.blocknumber, to=id.blocknumber)
      log = filter(lambda x: x.index == id.logIndex && x.address == id.origin)
      if len(log) == 0:
        return False

      if message != encode(log[0]):
        return False

      block = eth.getBlockByNumber(id.blocknumber)

      if id.timestamp != block.timestamp:
        return False

return True
```

[tx-to]: https://github.com/ethereum/execution-specs/blob/1fed0c0074f9d6aab3861057e1924411948dc50b/src/ethereum/frontier/fork_types.py#L52

#### Transitive-dependency confirmation

When operating pessimistically, the direct-dependency validation may be applied recursively,
to harden against unstable message guarantees at the cost of increased cross-chain latency.

The transitive dependencies may also be enforced with `safe` or even `finalized` safety-views
over the blocks of the chains in the dependency set, to further ensure increased cross-chain safety.

## Sponsorship

If a user does not have ether to pay for the gas of an executing message, application layer sponsorship
solutions can be created. It is possible to create an MEV incentive by paying `tx.origin` in the executing
message. This can be done by wrapping the `L2ToL2CrossDomainMessenger` with a pair of relaying contracts.

## Security Considerations

### Cross Chain Message Latency

The latency at which a cross chain message is relayed from the moment at which it was initiated is bottlenecked by
the security of the preconfirmations. An initiating transaction and a executing transaction MAY have the same timestamp,
meaning that a secure preconfirmation scheme enables atomic cross chain composability. Any sort of equivocation on
behalf of the sequencer will result in the production of invalid blocks.
