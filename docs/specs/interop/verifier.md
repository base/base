# Verifier

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Derivation Pipeline](#derivation-pipeline)
  - [Depositing an Executing Message](#depositing-an-executing-message)
  - [Safety](#safety)
    - [`unsafe` Inputs](#unsafe-inputs)
    - [`cross-unsafe` Inputs](#cross-unsafe-inputs)
    - [`safe` Inputs](#safe-inputs)
    - [`finalized` Inputs](#finalized-inputs)
  - [Honest Verifier](#honest-verifier)
- [Security Considerations](#security-considerations)
  - [Forced Inclusion of Cross Chain Messages](#forced-inclusion-of-cross-chain-messages)
    - [What if Safety isn't Enough?](#what-if-safety-isnt-enough)
  - [Reliance on History](#reliance-on-history)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Derivation Pipeline

The derivation pipeline enforces invariants on safe blocks that include executing messages.

- The executing message MUST have a corresponding initiating message
- The initiating message that corresponds to an executing message MUST come from a chain in its dependency set
- A block MUST be considered invalid if it is built with any invalid executing messages

Blocks that contain transactions that relay cross domain messages to the destination chain where the
initiating transaction does not exist MUST be considered invalid and MUST not be allowed by the
derivation pipeline to be considered safe.

There is no concept of replay protection at the lowest level of abstractions within the protocol because
there is no replay protection mechanism that fits well for all applications. Users MAY submit an
arbitrary number of executing messages per initiating message. Applications MUST build their own replay
protection mechanisms if they are interacting with the lowest level abstractions.

Blocks that contain invalid executing messages are considered invalid by the protocol. The derivation
pipeline will never promote them from being `unsafe`. A block that contains invalid executing messages
MUST be replaced by a deposits only block at the same block number.

### Depositing an Executing Message

Deposit transactions (force inclusion transactions) give censorship resistance to layer two networks.
The derivation pipeline must gracefully handle the case in which a user uses a deposit transaction to
relay a cross chain message. To not couple preconfirmation security to consensus, deposit transactions
that execute cross chain messages MUST have an initiating message that is considered [safe](#safety) by the remote
chain's derivation pipeline. This relaxes a strict synchrony assumption on the sequencer
that it MUST have all unsafe blocks of destination chains as fast as possible to ensure that it is building
correct blocks.

If a deposit transaction references an initiating transaction that is not yet safe or does not exist,
it MUST be dropped by the derivation pipeline.

This inclusion property prevents a class of attacks where the user can trick the derivation pipeline
into reorganizing the sequencer.

### Safety

Safety is an abstraction that is useful for reasoning about security. It should be thought about
as a spectrum from `unsafe` to `finalized`. Users can choose to operate on information based on its
level of safety depending on their risk profile and personal preferences.

The following labels are used to describe both inputs and outputs:

- `unsafe`
- `cross-unsafe`
- `safe`
- `finalized`

Inputs correspond to the inputs to the state transition function while outputs correspond to the side
effects of the state transition function.

Anything before `safe` technically uses a "preconfirmation" based security model which is not part
of consensus. While useful to have definitions of the default meanings of these terms, they are
technically policy and may be changed in the future.

The `unsafe` label has the lowest latency while the `finalized` label has the highest latency.
A set of invariants must be held true before an input or an output can be promoted to the next
label, starting at `unsafe`.

The initiating messages for all dependent executing messages MUST be resolved as safe before an L2 block can transition
from being unsafe to safe. Users MAY optimistically accept unsafe blocks without any verification of the
executing messages. They SHOULD optimistically verify the initiating messages exist in destination unsafe blocks
to more quickly reorganize out invalid blocks.

#### `unsafe` Inputs

- MUST be signed by the p2p sequencer key
- MAY be reorganized
- MUST be promoted to a higher level of safety or reorganized out to ensure liveness

`unsafe` inputs are currently gossiped around the p2p network. To prevent denial of service, they MUST
be signed by the sequencer. This signature represents the sequencer's claim that it
built a block that conforms to the protocol. `unsafe` blocks exist to give low latency access to the
latest information. To keep the latency as low as possible, cross chain messages are assumed valid
at this stage. This means that the remote unsafe inputs are trusted solely because they were
included in a block by the sequencer.

An alternative approach to `unsafe` inputs would be to include an SGX proof that the sequencer ran
particular software when building the block.

#### `cross-unsafe` Inputs

- MUST have valid cross chain messages

`cross-unsafe` represents the `unsafe` blocks that had their cross chain messages fully verified.
The network can be represented as a graph where each block across all chains are represented as
a node and then a directed edge between two blocks represents the source block of the initiating
message and the block that included the executing message.

An input can be promoted from `unsafe` to `cross-unsafe` when the full dependency graph is resolved
such that all cross chain messages are verified to be valid and at least one message in the dependency
graph is still `unsafe`.

Note that the `cross-unsafe` is not meant to be exposed to the end user via the RPC label. It is
meant for internal usage. All `cross-unsafe` inputs are still considered `unsafe` by the execution
layer RPC.

#### `safe` Inputs

- MUST be available
- MAY be reorganized
- Safe block MUST be invalidated if a reorg occurs

`safe` represents the state in which the `cross-unsafe` dependency graph has been fully resolved
in a way where all of the data has been published to the data availability layer.

#### `finalized` Inputs

- MUST NOT be reorganized based on Ethereum economic security

`finalized` represents full Proof of Stake economic security on top of the data. This means that
if the data is reorganized, then validators will be slashed.

### Honest Verifier

The honest verifier follows a naive verification algorithm. That is similar
to the block building code that the [sequencer](./sequencer.md#direct-dependency-confirmation)
follows. The main difference is that the validity of included executing
messages is verified instead of verifying possible executing messages before
inclusion.

## Security Considerations

### Forced Inclusion of Cross Chain Messages

The design is particular to not introduce any sort of "forced inclusion" between L2s. This design space introduces
risky synchrony assumptions and forces the introduction of a message queue to prevent denial of service attacks where
all chains in the network decide to send cross chain messages to the same chain at the same time.

"Forced inclusion" transactions are good for censorship resistance. In the worst case of censoring sequencers, it will
take at most 2 sequencing windows for the cross chain message to be processed. The initiating transaction can be sent
via a deposit which MUST be included in the source chain or the sequencer will be reorganized at the end of the
sequencing window that includes the deposit transaction. If the executing transaction is censored, it will take
another sequencing window of time to force the inclusion of the executing message per the
[spec][depositing-an-executing-message].

[depositing-an-executing-message]: #depositing-an-executing-message

#### What if Safety isn't Enough?

It is possible that small latency differences may impact the allowance of deposited executing messages
if the rule is that the initiating message is safe. A more strict invariant may be introduced:

```text
identifier.timestamp + sequencer_window <= block.timestamp
```

This says that a sequencer window must elapse before the initiating message can be referenced
in an executing message.

### Reliance on History

When fully executing historical blocks, a dependency on historical receipts from remote chains is present.
[EIP-4444][eip-4444] will eventually provide a solution for making historical receipts available without
needing to require increasingly large execution client databases.

It is also possible to introduce a form of expiry on historical receipts by enforcing that the timestamp
is recent enough in the `CrossL2Inbox`.

[eip-4444]: https://eips.ethereum.org/EIPS/eip-4444
