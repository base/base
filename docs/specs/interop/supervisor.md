# Supervisor

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [RPC API](#rpc-api)
  - [Common types](#common-types)
    - [`Identifier`](#identifier)
    - [`Message`](#message)
    - [`ExecutingDescriptor`](#executingdescriptor)
    - [`HexUint64`](#hexuint64)
    - [`Int`](#int)
    - [`ChainID`](#chainid)
    - [`Hash`](#hash)
    - [`Bytes`](#bytes)
    - [`BlockID`](#blockid)
    - [`BlockRef`](#blockref)
    - [`DerivedIDPair`](#derivedidpair)
    - [`ChainRootInfo`](#chainrootinfo)
    - [`SupervisorSyncStatus`](#supervisorsyncstatus)
    - [`SupervisorChainSyncStatus`](#supervisorchainsyncstatus)
    - [`SuperRootResponse`](#superrootresponse)
    - [`SafetyLevel`](#safetylevel)
  - [Methods](#methods)
    - [`supervisor_crossDerivedToSource`](#supervisor_crossderivedtosource)
    - [`supervisor_localUnsafe`](#supervisor_localunsafe)
    - [`supervisor_crossSafe`](#supervisor_crosssafe)
    - [`supervisor_finalized`](#supervisor_finalized)
    - [`supervisor_finalizedL1`](#supervisor_finalizedl1)
    - [`supervisor_superRootAtTimestamp`](#supervisor_superrootattimestamp)
    - [`supervisor_syncStatus`](#supervisor_syncstatus)
    - [`supervisor_allSafeDerivedAt`](#supervisor_allsafederivedat)
    - [`supervisor_checkAccessList`](#supervisor_checkaccesslist)
      - [Access-list contents](#access-list-contents)
      - [Access-list execution context](#access-list-execution-context)
      - [Access-list checks](#access-list-checks)
      - [`supervisor_checkAccessList` contents](#supervisor_checkaccesslist-contents)
  - [Errors](#errors)
    - [JSON-RPC Error Codes](#json-rpc-error-codes)
    - [Error Code Structure](#error-code-structure)
    - [Protocol Specific Error Codes](#protocol-specific-error-codes)
      - [`-3204XX` `DEADLINE_EXCEEDED` errors](#-3204xx-deadline_exceeded-errors)
        - [`-320400` `UNINITIALIZED_CHAIN_DATABASE`](#-320400-uninitialized_chain_database)
      - [`-3205XX` `NOT_FOUND` errors](#-3205xx-not_found-errors)
        - [`-320500` `SKIPPED_DATA`](#-320500-skipped_data)
        - [`-320501` `UNKNOWN_CHAIN`](#-320501-unknown_chain)
      - [`-3206XX` `ALREADY_EXISTS` errors](#-3206xx-already_exists-errors)
        - [`-320600` `CONFLICTING_DATA`](#-320600-conflicting_data)
        - [`-320601` `INEFFECTIVE_DATA`](#-320601-ineffective_data)
      - [`-3209XX` `FAILED_PRECONDITION` errors](#-3209xx-failed_precondition-errors)
        - [`-320900` `OUT_OF_ORDER`](#-320900-out_of_order)
        - [`-320901` `AWAITING_REPLACEMENT_BLOCK`](#-320901-awaiting_replacement_block)
      - [`-3210XX` `ABORTED` errors](#-3210xx-aborted-errors)
        - [`-321000` `ITER_STOP`](#-321000-iter_stop)
      - [`-3211XX` `OUT_OF_RANGE` errors](#-3211xx-out_of_range-errors)
        - [`-321100` `OUT_OF_SCOPE`](#-321100-out_of_scope)
      - [`-3212XX` `UNIMPLEMENTED` errors](#-3212xx-unimplemented-errors)
        - [`-321200` `CANNOT_GET_PARENT_OF_FIRST_BLOCK_IN_DB`](#-321200-cannot_get_parent_of_first_block_in_db)
      - [`-3214XX` `UNAVAILABLE` errors](#-3214xx-unavailable-errors)
        - [`-321401` `FUTURE_DATA`](#-321401-future_data)
      - [`-3215XX` `DATA_LOSS` errors](#-3215xx-data_loss-errors)
        - [`-321500` `MISSED_DATA`](#-321500-missed_data)
        - [`-321501` `DATA_CORRUPTION`](#-321501-data_corruption)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

The supervisor is an implementation detail of OP Stack native interop and is not the only
architecture that can be used to implement it. The supervisor is responsible for indexing
data from all of the chains in a cluster so that the [safety level](./verifier.md#safety)
of executing messages can quickly be determined.

## RPC API

### Common types

#### `Identifier`

The identifier of a message.
Corresponds to an [Identifier](./messaging.md#message-identifier).

Object:
- `origin`: `Address`
- `blockNumber`: `HexUint64`
- `logIndex`: `HexUint64`
- `timestamp`: `HexUint64`
- `chainID`: `ChainID`

#### `Message`

Describes an initiating message.

Object:
- `identifier`: `Identifier` - identifier of the message
- `payloadHash`: `Hash` - `keccak256` hash of the message-payload bytes

#### `ExecutingDescriptor`

Describes the context for message verification.
Specifically, this helps apply message-expiry rules on message checks.

Object:
- `timestamp`: `HexUint64` - expected timestamp during message execution.
- `timeout`: `HexUint64` - optional, requests verification to still hold at `timestamp+timeout` (inclusive).
  The message expiry-window may invalidate messages.
  Default interpretation is a `0` timeout: what is valid at `timestamp` may not be valid at `timestamp+1`.

#### `HexUint64`

`STRING`:
Hex-encoded big-endian number, variable length up to 64 bits, prefixed with `0x`.

#### `Int`

`NUMBER`:
Regular JSON number, always integer. Assumed to always fit in 51 bits.

#### `ChainID`

`STRING`:
Hex-encoded big-endian number, variable length up to 256 bits, prefixed with `0x`.

#### `Hash`

`STRING`:
Hex-encoded, fixed-length, representing 32 bytes, prefixed with `0x`.

#### `Bytes`

`STRING`:
Hex-encoded, variable length (always full bytes, no odd number of nibbles),
representing a bytes list, prefixed with `0x`.

#### `BlockID`

Describes a block.

`OBJECT`:
- `hash`: `Hash` - block hash
- `number`: `Int` - block number

#### `BlockRef`

Describes a block.

`OBJECT`:
- `hash`: `Hash` - block hash
- `number`: `Int` - block number
- `parentHash`: `Hash` - block parent-hash
- `timestamp`: `Int` - block timestamp

#### `DerivedIDPair`

A pair of block identifiers, linking a `derived` block to the `source` block from which it was derived.

`OBJECT`:

- `source`: `BlockID` - The block ID of the `source` block.
- `derived`: `BlockID` - The block ID of the `derived` block.

#### `ChainRootInfo`

`OBJECT`:
- `chainId`: `HexUint64` - The chain ID (Note: this is changing to `ChainID` soon)
- `canonical`: `Hash` - output root at the latest canonical block
- `pending`: `Bytes` - output root preimage

#### `SupervisorSyncStatus`

Describes the sync status of the Supervisor component. Fields `minSyncedL1`,
`safeTimestamp` and `finalizedTimestamp` are set as the minimum value among
chains in `chains`.

`OBJECT`:
- `minSyncedL1`: `BlockRef` - block ref to the synced L1 block
- `safeTimestamp`: `Int` - safe timestamp
- `finalizedTimestamp`: `Int` - finalized timestamp
- `chains`: `OBJECT` with `ChainID` keys and `SupervisorChainSyncStatus` values

> **Note:**  
> If `minSyncedL1` does not exist, it MUST be represented as a `BlockRef` with a `hash` and `parentHash` of  
> `0x0000000000000000000000000000000000000000000000000000000000000000`, a `number` of `0`,  
> and a `timestamp` of `0`.

#### `SupervisorChainSyncStatus`

Describes the sync status for a specific chain

`OBJECT`:
- `localUnsafe`: `BlockRef` - local-unsafe ref for the given chain
- `localSafe`: `BlockID` - local-safe ref for the given chain
- `crossUnsafe`: `BlockID` - cross-unsafe ref for the given chain
- `safe`: `BlockID` - cross-safe ref for the given chain
- `finalized`: `BlockID` - finalized ref for the given chain

> **Note:**  
> For the fields `localSafe`, `crossUnsafe`, `safe`, and `finalized`, if the referenced block does not exist yet,  
> the `BlockID` MUST be represented with a `hash` of  
> `0x0000000000000000000000000000000000000000000000000000000000000000` and a `number` of `0`.

#### `SuperRootResponse`

`OBJECT`:
- `crossSafeDerivedFrom`: `BlockID` - common derived-from where all chains are cross-safe
- `timestamp`: `Int` - The timestamp of the super root
- `superRoot`: `Hash` - The root of the super root
- `version`: `Int` - The version of the response
- `chains`: `ARRAY` of `ChainRootInfo` - List of chains included in the super root

#### `SafetyLevel`

The safety level of the message.
Corresponds to a verifier [SafetyLevel](./verifier.md#safety).

`STRING`, one of:
- `invalid`
- `unsafe`: equivalent to safety of the `latest` RPC label.
- `cross-unsafe`
- `local-safe`
- `safe`: matching cross-safe, named `safe` to match the RPC label.
- `finalized`

### Methods

#### `supervisor_crossDerivedToSource`

Parameters:
- `chainID`: `ChainID`
- `derived`: `BlockID`

Returns: derivedFrom `BlockRef`

#### `supervisor_localUnsafe`

Parameters:
- `chainID`: `ChainID`

Returns: `BlockID`

#### `supervisor_crossSafe`

Parameters:
- `chainID`: `ChainID`

Returns: `DerivedIDPair`

#### `supervisor_finalized`

Parameters:
- `chainID`: `ChainID`

Returns: `BlockID`

#### `supervisor_finalizedL1`

Parameters: (none)

Returns: `BlockRef`

#### `supervisor_superRootAtTimestamp`

Retrieves the super root state at the specified timestamp,
which represents the global state across all monitored chains.

Parameters:
- `timestamp`: `HexUint64`

Returns: `SuperRootResponse`

#### `supervisor_syncStatus`

Parameters: (none)

Returns: `SupervisorSyncStatus`.
Throws: Some error if set of supervised chains is empty.

#### `supervisor_allSafeDerivedAt`

Returns the last derived block for each chain, from the given L1 block.

Parameters:
- `derivedFrom`: `BlockID`

Returns: derived blocks, mapped in a `OBJECT`:
- key: `ChainID`
- value: `BlockID`

#### `supervisor_checkAccessList`

Verifies if an access-list, as defined in [EIP-2930], references only valid messages.
Message execution in the [`CrossL2Inbox`] that is statically declared in the access-list will not revert.

[EIP-2930]: https://eips.ethereum.org/EIPS/eip-2930

##### Access-list contents

Only the [`CrossL2Inbox`] subset of the access-list in the transaction is required,
storage-access by other addresses is not included.

Note that an access-list can contain multiple different storage key lists for the `CrossL2Inbox` address.
All storage keys applicable to the `CrossL2Inbox` MUST be joined together (preserving ordering),
missing storage-keys breaks inbox safety.

**ALL storage-keys in the access-list for the `CrossL2Inbox` MUST be checked.**
If there is any unrecognized or invalid key, the access-list check MUST fail.

[`CrossL2Inbox`]: ./predeploys.md#crossl2inbox

##### Access-list execution context

The provided execution-context is used to determine validity relative to the provided time constraints,
see [timestamp invariants](./derivation.md#invariants).

Since messages expire, validity is not definitive.
To reserve validity for a longer time range, a non-zero `timeout` value can be used.
See [`ExecutingDescriptor`](#executingdescriptor) documentation.

As block-builder a `timeout` of `0` should be used.

As transaction pre-verifier, a `timeout` of `86400` (1 day) should be used.
The transaction should be re-verified or dropped after this time duration,
as it can no longer be safely included in the block due to message-expiry.

##### Access-list checks

The access-list check errors are not definite state-transition blockers, the RPC based checks can be extra conservative.
I.e. a message that is uncertain to meet the requested safety level may be denied.
Specifically, no attempt may be made to verify messages that are initiated and executed within the same timestamp,
these are `invalid` by default.
Advanced block-builders may still choose to include these messages by verifying the intra-block constraints.

##### `supervisor_checkAccessList` contents

Parameters:
- `inboxEntries`: `ARRAY` of `Hash` - statically declared `CrossL2Inbox` access entries.
- `minSafety`: `SafetyLevel` - minimum required safety, one of:
  - `unsafe`: the message exists.
  - `cross-unsafe`: the message exists in a cross-unsafe block.
  - `local-safe`: the message exists in a local-safe block, not yet cross-verified.
  - `safe`: the message exists in a derived block that is cross-verified.
  - `finalized`: the message exists in a finalized block.
  - Other safety levels are invalid and result in an error.
- `executingDescriptor`: `ExecutingDescriptor` - applies as execution-context to all messages.

Returns: RPC error if the `minSafety` is not met by one or more of the access entries.

The access-list entries represent messages, and may be incomplete or malformed.
Malformed access-lists result in an RPC error.

### Errors

The Supervisor RPC API uses a 6-digit error code system that extends standard JSON-RPC error codes
while providing additional categorization based on gRPC status codes.

For standard JSON-RPC error codes, refer to [Ethereum JSON-RPC Error Reference](https://ethereum-json-rpc.com/errors).

#### JSON-RPC Error Codes

#### Error Code Structure

Supervisor error codes follow this structure:

- First 2 digits: `32` (indicating server error, matching JSON-RPC conventions)
- Middle 2 digits: gRPC status code category (01-16, zero-padded)
- Last 2 digits: Specific error index within that category (00-99)

For gRPC status codes reference, see [gRPC Status Codes](https://grpc.io/docs/guides/status-codes/).

#### Protocol Specific Error Codes

##### `-3204XX` `DEADLINE_EXCEEDED` errors

###### `-320400` `UNINITIALIZED_CHAIN_DATABASE`

Happens when a chain database is not initialized yet.

##### `-3205XX` `NOT_FOUND` errors

###### `-320500` `SKIPPED_DATA`

Happens when we try to retrieve data that is not available (pruned).
It may also happen if we erroneously skip data, that was not considered a conflict, if the DB is corrupted.

###### `-320501` `UNKNOWN_CHAIN`

Happens when a chain is unknown, not in the dependency set.

##### `-3206XX` `ALREADY_EXISTS` errors

###### `-320600` `CONFLICTING_DATA`

Happens when we know for sure that there is different canonical data.

###### `-320601` `INEFFECTIVE_DATA`

Happens when data is accepted as compatible, but did not change anything.
This happens when a node is deriving an L2 block we already know of being derived from the given source,
but without path to skip forward to newer source blocks without doing the known derivation work first.

##### `-3209XX` `FAILED_PRECONDITION` errors

###### `-320900` `OUT_OF_ORDER`

Happens when you try to add data to the DB, but it does not actually fit onto the latest data
(by being too old or new).

###### `-320901` `AWAITING_REPLACEMENT_BLOCK`

Happens when we know for sure that a replacement block is needed before progress can be made.

##### `-3210XX` `ABORTED` errors

###### `-321000` `ITER_STOP`

Happens in iterator to indicate iteration has to stop.
This error might only be used internally and not sent over the network.

##### `-3211XX` `OUT_OF_RANGE` errors

###### `-321100` `OUT_OF_SCOPE`

Happens when data is accessed, but access is not allowed, because of a limited scope.
E.g. when limiting scope to L2 blocks derived from a specific subset of the L1 chain.

##### `-3212XX` `UNIMPLEMENTED` errors

###### `-321200` `CANNOT_GET_PARENT_OF_FIRST_BLOCK_IN_DB`

Happens when you try to get the previous block of the first block.
E.g. when trying to determine the previous source block for the first L1 block in the database.

##### `-3214XX` `UNAVAILABLE` errors

###### `-321401` `FUTURE_DATA`

Happens when data is just not yet available.

##### `-3215XX` `DATA_LOSS` errors

###### `-321500` `MISSED_DATA`

Happens when we search the DB, know the data may be there, but is not (e.g. different revision).

###### `-321501` `DATA_CORRUPTION`

Happens when the underlying DB has some I/O issue.
