# RPC

## L2 Output RPC method

The Rollup node has its own RPC method, `optimism_outputAtBlock` which returns a 32
byte hash corresponding to the [L2 output root](../proposals.md#l2-output-commitment-construction).

### Structures

These define the types used by rollup node API methods.
The types defined here are extended from the [engine API specs][engine-structures].

#### BlockID

- `hash`: `DATA`, 32 Bytes
- `number`: `QUANTITY`, 64 Bits

#### L1BlockRef

- `hash`: `DATA`, 32 Bytes
- `number`: `QUANTITY`, 64 Bits
- `parentHash`: `DATA`, 32 Bytes
- `timestamp`: `QUANTITY`, 64 Bits

#### L2BlockRef

- `hash`: `DATA`, 32 Bytes
- `number`: `QUANTITY`, 64 Bits
- `parentHash`: `DATA`, 32 Bytes
- `timestamp`: `QUANTITY`, 64 Bits
- `l1origin`: `BlockID`
- `sequenceNumber`: `QUANTITY`, 64 Bits - distance to first block of epoch

#### SyncStatus

Represents a snapshot of the rollup driver.

- `current_l1`: `Object` - instance of [`L1BlockRef`](#l1blockref).
- `current_l1_finalized`: `Object` - instance of [`L1BlockRef`](#l1blockref).
- `head_l1`: `Object` - instance of [`L1BlockRef`](#l1blockref).
- `safe_l1`: `Object` - instance of [`L1BlockRef`](#l1blockref).
- `finalized_l1`: `Object` - instance of [`L1BlockRef`](#l1blockref).
- `unsafe_l2`: `Object` - instance of [`L2BlockRef`](#l2blockref).
- `safe_l2`: `Object` - instance of [`L2BlockRef`](#l2blockref).
- `finalized_l2`: `Object` - instance of [`L2BlockRef`](#l2blockref).
- `pending_safe_l2`: `Object` - instance of [`L2BlockRef`](#l2blockref).
- `queued_unsafe_l2`: `Object` - instance of [`L2BlockRef`](#l2blockref).

### Output Method API

The input and return types here are as defined by the [engine API specs][engine-structures].

[engine-structures]: https://github.com/ethereum/execution-apis/blob/main/src/engine/paris.md#structures

- method: `optimism_outputAtBlock`
- params:
  1. `blockNumber`: `QUANTITY`, 64 bits - L2 integer block number.
- returns:
  1. `version`: `DATA`, 32 Bytes - the output root version number, beginning with 0.
  1. `outputRoot`: `DATA`, 32 Bytes - the output root.
  1. `blockRef`: `Object` - instance of [`L2BlockRef`](#l2blockref).
  1. `withdrawalStorageRoot`: 32 bytes - storage root of the `L2toL1MessagePasser` contract.
  1. `stateRoot`: `DATA`: 32 bytes - the state root.
  1. `syncStatus`: `Object` - instance of [`SyncStatus`](#syncstatus).
