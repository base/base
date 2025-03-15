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
- `hash`: `HASH` - block hash
- `number`: `Int` - block number

#### `BlockRef`

Describes a block.

`OBJECT`:
- `hash`: `Hash` - block hash
- `number`: `Int` - block number
- `parentHash`: `Hash` - block parent-hash
- `timestamp`: `Int` - block timestamp

#### `DerivedIDPair`

#### `ChainRootInfo`

`OBJECT`:
- `chainId`: `HexUint64` - The chain ID (Note: this is changing to `ChainID` soon)
- `canonical`: `Hash` - output root at the latest canonical block
- `pending`: `Bytes` - output root preimage

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

Returns: `SupervisorSyncStatus`

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
