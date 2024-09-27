# Messaging

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Message](#message)
  - [Message payload](#message-payload)
  - [Message Identifier](#message-identifier)
- [Messaging ends](#messaging-ends)
  - [Initiating Messages](#initiating-messages)
  - [Executing Messages](#executing-messages)
- [Messaging Invariants](#messaging-invariants)
  - [Timestamp Invariant](#timestamp-invariant)
  - [ChainID Invariant](#chainid-invariant)
  - [Message Expiry Invariant](#message-expiry-invariant)
- [Message Graph](#message-graph)
  - [Invalid messages](#invalid-messages)
    - [Block reorgs](#block-reorgs)
    - [Block Recommits](#block-recommits)
  - [Intra-block messaging: cycles](#intra-block-messaging-cycles)
  - [Resolving cross-chain safety](#resolving-cross-chain-safety)
  - [Horizon timestamp](#horizon-timestamp)
  - [Pruning the graph](#pruning-the-graph)
  - [Bounding the graph](#bounding-the-graph)
- [Security Considerations](#security-considerations)
  - [Cyclic dependencies](#cyclic-dependencies)
  - [Transitive dependencies](#transitive-dependencies)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Message

A message is a [broadcast payload](#message-payload) emitted from an [identified source](#message-identifier).

### Message payload

Opaque `bytes` that represent a [Log][log].

It is serialized by first concatenating the topics and then with the data.

```go
msg := make([]byte, 0)
for _, topic := range log.Topics {
    msg = append(msg, topic.Bytes()...)
}
msg = append(msg, log.Data...)
```

The `_msg` can easily be decoded into a Solidity `struct` with `abi.decode` since each topic is always 32 bytes and
the data generally ABI encoded.

### Message Identifier

[`Identifier`]: #message-identifier

The [`Identifier`] that uniquely represents a log that is emitted from a chain. It can be considered to be a
unique pointer to a particular log. The derivation pipeline and fault proof program MUST ensure that the
`_msg` corresponds exactly to the log that the [`Identifier`] points to.

```solidity
struct Identifier {
    address origin;
    uint256 blocknumber;
    uint256 logIndex;
    uint256 timestamp;
    uint256 chainid;
}
```

| Name          | Type      | Description                                                                     |
|---------------|-----------|---------------------------------------------------------------------------------|
| `origin`      | `address` | Account that emits the log                                                      |
| `blocknumber` | `uint256` | Block number in which the log was emitted                                       |
| `logIndex`    | `uint256` | The index of the log in the array of all logs emitted in the block              |
| `timestamp`   | `uint256` | The timestamp that the log was emitted. Used to enforce the timestamp invariant |
| `chainid`     | `uint256` | The chain id of the chain that emitted the log                                  |

The [`Identifier`] includes the set of information to uniquely identify a log. When using an absolute
log index within a particular block, it makes ahead of time coordination more complex. Ideally there
is a better way to uniquely identify a log that does not add ordering constraints when building
a block. This would make building atomic cross chain messages more simple by not coupling the
exact state of the block templates between multiple chains together.

## Messaging ends

### Initiating Messages

[log]: https://github.com/ethereum/go-ethereum/blob/5c67066a050e3924e1c663317fd8051bc8d34f43/core/types/log.go#L29

Each [Log][log] (also known as `event` in solidity) forms an initiating message.
The raw log data from the [Message Payload](#message-payload).

Messages are *broadcast*: the protocol does not enshrine address-targeting within messages.

The initiating message is uniquely identifiable with an [`Identifier`](#message-identifier),
such that it can be distinguished from other duplicate messages within the same transaction or block.

An initiating message may be executed many times: no replay-protection is enshrined in the protocol.

### Executing Messages

An executing message is represented by the [ExecutingMessage event][event] that is emitted by
the `CrossL2Inbox` predeploy. If the cross chain message is directly executed via [executeMessage](./predeploys.md#executemessage),
the event is coupled to a `CALL` with the payload that is emitted within the event to the target
address, allowing introspection of the data. Contracts can also introduce their own public
entrypoints and solely trigger validation of the cross chain message with [validateMessage](./predeploys.md#validatemessage).

All of the information required to satisfy the invariants MUST be included in this event.

[event]: ./predeploys.md#executingmessage-event

Both the block builder and the verifier use this information to ensure that all system invariants are held.

The executing message is verified by checking if there is an existing initiating-message
that originates at [`Identifier`] with matching [Message Payload](#message-payload).

Since an executing message is defined by a log, it means that reverting calls to the `CrossL2Inbox`
do not count as executing messages.

## Messaging Invariants

- [Timestamp Invariant](#timestamp-invariant): The timestamp at the time of inclusion of the initiating message MUST
  be less than or equal to the timestamp of the executing message as well as greater than or equal to the Interop Start Timestamp.
- [ChainID Invariant](#chainid-invariant): The chain id of the initiating message MUST be in the dependency set
- [Message Expiry Invariant](#message-expiry-invariant): The timestamp at the time of inclusion of the executing
  message MUST be lower than the initiating message timestamp (as defined in the [`Identifier`]) + `EXPIRY_TIME`.

### Timestamp Invariant

The timestamp invariant ensures that initiating messages is at least the same timestamp as the Interop upgrade timestamp
and cannot come from a future block than the block of its executing message. Note that since
all transactions in a block have the same timestamp, it is possible for an executing transaction to be
ordered before the initiating message in the same block.

### ChainID Invariant

Without a guarantee on the set of dependencies of a chain, it may be impossible for the derivation
pipeline to know which chain to source the initiating message from. This also allows for chain operators
to explicitly define the set of chains that they depend on.

### Message Expiry Invariant

Note: Message Expiry as property of the protocol is in active discussion.
It helps set a strict bound on total messaging activity to support, but also limits use-cases.
This trade-off is in review. This invariant may be ignored in initial interop testnets.

The expiry invariant invalidates inclusion of any executing message with
`id.timestamp + EXPIRY_TIME < executing_block.timestamp` where:

- `id` is the [`Identifier`] encoded in the executing message, matching the block attributes of the initiating message.
- `executing_block` is the block where the executing message was included in.
- `EXPIRY_TIME = 180 * 24 * 60 * 60 = 15552000` seconds, i.e. 180 days.

## Message Graph

The dependencies of messages can be modeled as a directed graph:

- **vertex**: a block
- **edge**: a dependency:
  - parent-block (source) to block (target)
  - message relay, from initiation (source) to execution (target)

If the source of an edge is invalidated, the target is invalidated.

### Invalid messages

A message is said to be "invalid" when the executing message does not have a valid dependency.

Dependencies are invalid when:

- The dependency is unknown.
- The dependency is known but not part of the canonical chain.
- The dependency is known but does not match all message attributes ([`Identifier`] and payload).

#### Block reorgs

The [`Identifier`] used by an executing message does not cryptographically
commit to the block it may reference.

Messages may be executed before the block that initiates them is sealed.

When tracking message dependencies, edges are maintained for *all* identified source blocks.

Reorgs are resolved by filtering the view of the solver to only canonical blocks.
If the source block is not canonical, the dependency is invalid.

The canonical L2 block at the identified block-height is the source of truth.

#### Block Recommits

Blocks may be partially built, or sealed but not published, and then rebuilt.

Any optimistically assumed messages have to be revalidated,
and the original partial block can be removed from the dependency graph.

### Intra-block messaging: cycles

While messages cannot be initiated by future blocks,
they can be initiated by any transactions within the same timestamp,
as per the [Timestamp Invariant](#timestamp-invariant).

This property allows messages to form cycles in the graph:
blocks with equal timestamp, of chains in the same dependency set,
may have dependencies on one another.

### Resolving cross-chain safety

To determine cross-chain safety, the graph is inspected for valid graph components that have no invalid dependencies,
while applying the respective safety-view on the blocks in the graph.

I.e. the graph must not have any inward edges towards invalid blocks within the safety-view.

A safety-view is the subset of canonical blocks of all chains with the specified safety label or a higher safety label.
Dependencies on blocks outside of the safety-view are invalid,
but may turn valid once the safety-view changes (e.g. a reorg of unsafe blocks).

By resolving in terms of graph-components, cyclic dependencies and transitive dependencies are addressed.

### Horizon timestamp

The maximum seen timestamp in the graph is the `horizon_timestamp`.

The verifier can defer growth of the graph past this `horizon_timestamp`,
until the graph has been resolved and pruned.

### Pruning the graph

Edges between blocks can be de-duplicated, when the blocks are complete and sealed.

The same initiating or executing messages, but attached to different blocks,
may have to be added back to the graph if the set of blocks changes.

Blocks with cross-chain safety in the finalized-blocks safety-view can be optimized:
outward edges for initiating messages may be attached
when relevant to resolution of newer lower-safety blocks.
No inward edges are required anymore however, as the maximum safety has been ensured.

Blocks older than `horizon_timestamp - (2 * EXPIRY_TIME)` cannot be depended
on from any valid block within the graph, and can thus be safely pruned entirely.

### Bounding the graph

With many events, and transitive dependencies, resolving the cross-chain safety may be an intensive task for a verifier.
It is thus important for the graph to be reasonably bounded, such that it can be resolved.

The graph is bounded in 4 ways:

- Every block can only depend on blocks of chains in the dependency set,
  as per the [ChainID invariant](#chainid-invariant).
- Every block cannot depend on future blocks, as per the [Timestamp invariant](#timestamp-invariant).
- Every block has a maximum gas limit, an intrinsic cost per transaction,
  and thus a maximum inward degree of dependencies.
- Every block cannot depend on expired messages, as per the [Message expiry invariant](#message-expiry-invariant).

The verifier is responsible for filtering out non-canonical parts of the graph.

## Security Considerations

### Cyclic dependencies

If there is a cycle in the dependency set, chains MUST still be able to promote unsafe blocks
to safe blocks. A cycle in the dependency set happens anytime that two chains are in each other's
dependency set. This means that they are able to send cross chain messages to each other.

### Transitive dependencies

The safety of a chain is only ensured when the inputs are safe:
dependencies are thus said to be transitive.
Without validating the transitive dependencies,
the verifier relies on the verification work of the nodes that it sources its direct dependencies from.
