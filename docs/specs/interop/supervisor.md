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
    - [`supervisor_checkMessage`](#supervisor_checkmessage)
    - [`supervisor_checkMessages`](#supervisor_checkmessages)
    - [`supervisor_checkMessagesV2`](#supervisor_checkmessagesv2)
    - [`supervisor_crossDerivedToSource`](#supervisor_crossderivedtosource)
    - [`supervisor_localUnsafe`](#supervisor_localunsafe)
    - [`supervisor_crossSafe`](#supervisor_crosssafe)
    - [`supervisor_finalized`](#supervisor_finalized)
    - [`supervisor_finalizedL1`](#supervisor_finalizedl1)
    - [`supervisor_superRootAtTimestamp`](#supervisor_superrootattimestamp)
    - [`supervisor_syncStatus`](#supervisor_syncstatus)
    - [`supervisor_allSafeDerivedAt`](#supervisor_allsafederivedat)

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
- `timestamp`: `HexUint64`

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
- `unsafe`
- `cross-unsafe`
- `local-safe`
- `safe`
- `finalized`

### Methods

#### `supervisor_checkMessage`

Checks the safety level of a specific message based on its identifier and message hash.
This RPC is useful for the block builder to determine if a message should be included in a block.

Parameters:
- `identifier`: `Identifier`
- `payloadHash`: `Hash`
- `executingDescriptor`: `ExecutingDescriptor`

Returns: `SafetyLevel`

#### `supervisor_checkMessages`

Parameters:
- `messages`: ARRAY of `Message`
- `minSafety`: `SafetyLevel`

#### `supervisor_checkMessagesV2`

Next version `supervisor_checkMessage`,
additionally verifying the message-expiry, by referencing when the execution happens.

Parameters:
- `messages`: ARRAY of `Message`
- `minSafety`: `SafetyLevel`
- `executingDescriptor`: `ExecutingDescriptor` - applies as execution-context to all messages

Returns: RPC error the minSafety is not met by one or more of the messages, with

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
