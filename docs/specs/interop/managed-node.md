# Control flow between Supervisor and Managed node

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Authentication](#authentication)
- [Node `->` Supervisor](#node---supervisor)
  - [Reset](#reset)
  - [UnsafeBlock](#unsafeblock)
  - [DerivationUpdate](#derivationupdate)
  - [DerivationOriginUpdate](#derivationoriginupdate)
  - [ExhaustL1](#exhaustl1)
  - [ReplaceBlock](#replaceblock)
- [Supervisor `->` Node](#supervisor---node)
  - [Control Signals](#control-signals)
    - [interop_pullEvent](#interop_pullevent)
    - [interop_anchorPoint (Soon to be deprecated)](#interop_anchorpoint-soon-to-be-deprecated)
    - [interop_invalidateBlock](#interop_invalidateblock)
    - [interop_provideL1](#interop_providel1)
    - [interop_reset](#interop_reset)
  - [DB](#db)
    - [interop_updateCrossSafe](#interop_updatecrosssafe)
    - [interop_updateCrossUnsafe](#interop_updatecrossunsafe)
    - [interop_updateFinalized](#interop_updatefinalized)
  - [Sync Methods](#sync-methods)
    - [interop_fetchReceipts](#interop_fetchreceipts)
    - [interop_l2BlockRefByTimestamp](#interop_l2blockrefbytimestamp)
    - [interop_blockRefByNumber](#interop_blockrefbynumber)
    - [interop_chainID](#interop_chainid)
    - [interop_outputV0AtTimestamp](#interop_outputv0attimestamp)
    - [interop_pendingOutputV0AtTimestamp](#interop_pendingoutputv0attimestamp)
- [Types](#types)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

This section is based on the control flow between `managed` node and `supervisor`, described
[here](https://github.com/ethereum-optimism/optimism/blob/develop/op-supervisor/README.md).

Whether the node and supervisor use a `pubsub` pattern or pulling events over HTTP is up to the implementation detail.
Both cases should be handled because `op-node` and `op-supervisor` can be using different client implementations.
In both the case, we use `event` based signaling of new information. Ideally, this should be done using bi-directional
RPC using web sockets.

## Authentication

Supervisor [initiates the connection](https://github.com/ethereum-optimism/optimism/issues/13182) to the node in order
to manage it. As supervisor does the dial in, for our context, it is the client and the node is the server. For the web
socket authentication, only the initial request is authenticated and a live socket is opened.
No re-authentication is needed during the lifetime of this socket.

The authentication is performed using `JWT`. The construction of jwt is the same to the one
[used in Engine API](https://github.com/ethereum/execution-apis/blob/main/src/engine/authentication.md). The path of
the file containing the jwt secret should be provided to the supervisor instance.

## Node `->` Supervisor

Events that a supervisor should subscribe to, originating from the node, handled by the supervisor. For the used types,
refer [this](#Types) section.

Every event sent from the node is of type `ManagedEvent` whose fields are populated with the events that occurred. All
non-null events are sent at once. The other fields are omitted.

```javascript
ManagedEvent {
    Reset,
    UnsafeBlock,
    DerivationUpdate,
    DerivationOriginUpdate,
    ExhaustL1,
    ReplaceBlock
}
```

Each field item is an event of the type as described below:

### Reset

```javascript
Reset: string
```

This is emitted when the node has determined that it needs a reset. It tells the supervisor to send the
[interop_reset](#interop_reset) event with the required parameters.

### UnsafeBlock

```javascript
UnsafeBlock: BlockRef //L2's BlockRef
```

New L2 unsafe block was processed, updating local-unsafe head.

### DerivationUpdate

```javascript
DerivationUpdate: DerivedBlockRefPair {
    source: BlockRef  //L1 
    derived: BlockRef //Last L2 BlockRef
}
```

Signals that an L2 block is considered local-safe.

### DerivationOriginUpdate

```javascript
DerivationOriginUpdate: BlockRef
```

Signals that an L2 block is now local-safe because of the given L1 traversal. This would be accompanied with
`DerivationUpdate`.

### ExhaustL1

```javascript
ExhaustL1: DerivedBlockRefPair {
    source: BlockRef  //Last L1
    derived: BlockRef //Last L2 BlockRef
}
```

Emitted when no more L1 Blocks are available. Ready to take new L1 blocks from supervisor.

### ReplaceBlock

```javascript
ReplaceBlock: BlockReplacement
```

Emitted when a block gets replaced for any reason.

## Supervisor `->` Node

Messages that a node should watch for, originating from supervisor, handled by node. This also includes control signals
that the supervisor can ask the node to perform. For the used types, refer [this](#types) section.

*Note: Headings represent the actual name of the methods over RPC, payload is the serialized version of the given type.*

`-> indicates return`

### Control Signals

RPC calls that are relevant to managing the node via supervisor. These are directly called by the supervisor.

#### interop_pullEvent

```javascript
payload() -> One of the event from node events (previous section).
```

When websocket is not available, supervisor can use this method to get the next event from node.

#### interop_anchorPoint (Soon to be deprecated)

```javascript
payload() -> DerivedBlockRefPair {
    source: L1 BlockRef that rollup starts after (no derived transaction)
    derived: L2 BlockRef that rollup starts from (no txn, pre-configuration state)
}
```

Returns the genesis block ref for L1 and L2 that the current node used as anchor point.
This method will soon be removed in favor of fetching the information from a supervisor specific rollup config.
(Once a spec is created about [this](https://github.com/ethereum-optimism/optimism/pull/16038) PR.)

#### interop_invalidateBlock

```javascript
payload (BlockSeal)  //L2's Block
```

Based on some dependency or L1 changes, supervisor can instruct the L2 to invalidate a specific block.

*(Suggestion: BlockSeal can be replaced with Hash, as only that is being used in op-node.)*

#### interop_provideL1

```javascript
payload (BlockRef) //L1 Block
```

Supervisor sends the next L1 block to the node. Ideally sent after the node emits `exhausted-l1`.

#### interop_reset

```javascript
payload (lUnsafe, xUnsafe, lSafe, xSafe, finalized: BlockID)
```

Forces a reset to a specific local-unsafe/local-safe/finalized starting point only if the blocks did exist. Resets may
override local-unsafe, to reset the very end of the chain. Resets may override local-safe, since post-interop we need
the local-safe block derivation to continue.

### DB

RPC calls that a node should watch for, originating from supervisor that is called on DB updates for relevant block
safety info for a given chain and block.

#### interop_updateCrossSafe

```javascript
payload (derived: BlockID, source: BlockID)
```

Signal that a block can be promoted to cross-safe.

#### interop_updateCrossUnsafe

```javascript
payload (BlockID)
```

Signal that a block can be promoted to cross-unsafe.

#### interop_updateFinalized

```javascript
payload (BlockID)
```

Signal that a block can be marked as finalized.

### Sync Methods

RPC methods that are relevant for syncing the supervisor. These are directly called by the supervisor for fetching L2
data.

#### interop_fetchReceipts

```javascript
payload (Hash) -> Receipts //L2 block hash
```

Fetches all transaction receipts in a given L2 block.

#### interop_l2BlockRefByTimestamp

```javascript
payload (uint64) -> BlockRef
```

Fetches L2 BlockRef of the block that occurred at given timestamp

#### interop_blockRefByNumber

```javascript
payload (uint64) -> BlockRef
```

Fetches the BlockRef from a given L2 block number

#### interop_chainID

```javascript
payload () -> string
```

Returns chainID of the L2

#### interop_outputV0AtTimestamp

```javascript
payload (uint64) -> OutputV0
```

Returns the state root, storage root and block hash for a given timestamp

#### interop_pendingOutputV0AtTimestamp

```javascript
payload (uint64) -> OutputV0
```

Returns the optimistic output of the invalidated block from replacement

**Note:** All events should return relevant error(s) in case of unsuccessful calls.

## Types

```javascript
Hash: 0x prefixed, hex encoded, fixed-length string representing 32 bytes

BlockID {
    hash: Hash
    number: uint64
}

BlockSeal {
    hash: Hash
    number: uint64
    timestamp: uint64
}

BlockRef {
    hash: Hash
    number: uint64
    parentHash: Hash
    timestamp: uint64
}

DerivedBlockRefPair {
    source: BlockRef
    derived: BlockRef
}

BlockReplacement {
    replacement: BlockRef
    invalidated: Hash
}

Receipts -> []Receipt

Receipt -> op-geth/core/types/receipt

OutputV0 {
    stateRoot: Hash
    messagePasserStorageRoot: Hash
    blockHash: Hash
}
```
