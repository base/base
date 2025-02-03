# L2 Execution Engine

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Timestamp Activation](#timestamp-activation)
- [`L2ToL1MessagePasser` Storage Root in Header](#l2tol1messagepasser-storage-root-in-header)
  - [Header Validity Rules](#header-validity-rules)
  - [Header Withdrawals Root](#header-withdrawals-root)
    - [Rationale](#rationale)
    - [Genesis Block](#genesis-block)
    - [State Processing](#state-processing)
    - [P2P](#p2p)
    - [Backwards Compatibility Considerations](#backwards-compatibility-considerations)
    - [Forwards Compatibility Considerations](#forwards-compatibility-considerations)
    - [Client Implementation Considerations](#client-implementation-considerations)
      - [Transaction Simulation](#transaction-simulation)
- [Deposit Requests](#deposit-requests)
- [Block Body Withdrawals List](#block-body-withdrawals-list)
- [EVM Changes](#evm-changes)
- [Block Sealing](#block-sealing)
- [Engine API Updates](#engine-api-updates)
  - [Update to `ExecutableData`](#update-to-executabledata)
  - [`engine_newPayloadV4` API](#engine_newpayloadv4-api)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

<!-- All glossary references in this file. -->

[l2-to-l1-mp]: ../../protocol/predeploys.md#L2ToL1MessagePasser
[output-root]: ../../glossary.md#l2-output-root

## Overview

The storage root of the `L2ToL1MessagePasser` is included in the block header's
`withdrawalRoot` field.

## Timestamp Activation

Isthmus, like other network upgrades, is activated at a timestamp.
Changes to the L2 Block execution rules are applied when the `L2 Timestamp >= activation time`.

## `L2ToL1MessagePasser` Storage Root in Header

After Isthmus hardfork's activation, the L2 block header's `withdrawalsRoot` field will consist of the 32-byte
[`L2ToL1MessagePasser`][l2-to-l1-mp] account storage root from the world state identified by the stateRoot
field in the block header. The storage root should be the same root that is returned by `eth_getProof`
at the given block number.

### Header Validity Rules

Prior to isthmus activation:

- the L2 block header's `withdrawalsRoot` field must be:
  - `nil` if Canyon has not been activated.
  - `keccak256(rlp(empty_string_code))` if Canyon has been activated.
- the L2 block header's `requestsHash` field must be omitted.

After Isthmus activation, an L2 block header is valid iff:

1. The `withdrawalsRoot` field
    1. Is 32 bytes in length.
    1. Matches the [`L2ToL1MessagePasser`][l2-to-l1-mp] account storage root,
    as committed to in the `storageRoot` within the block header
1. The `requestsHash` field is equal to `sha256('') = 0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`
indicating no requests in the block.

### Header Withdrawals Root

| Byte offset | Description                                               |
| ----------- | --------------------------------------------------------- |
| `[0, 32)`   | [`L2ToL1MessagePasser`][l2-to-l1-mp] account storage root |

#### Rationale

Currently, to generate [L2 output roots][output-root] for historical blocks, an archival node is required. This directly
places a burden on users of the system in a post-fault-proofs world, where:

1. A proposer must have an archive node to propose an output root at the safe head.
1. A user that is proving their withdrawal must have an archive node to verify that the output root they are proving
   their withdrawal against is indeed valid and included within the safe chain.

Placing the [`L2ToL1MessagePasser`][l2-to-l1-mp] account storage root in the `withdrawalsRoot` field alleviates this burden
for users and protocol participants alike, allowing them to propose and verify other proposals with lower operating costs.

#### Genesis Block

If Isthmus is active at genesis block, the `withdrawalsRoot` in the genesis block header is set to the
[`L2ToL1MessagePasser`][l2-to-l1-mp] account storage root.

#### State Processing

At the time of state processing, the header for which transactions are being validated should not make it's `withdrawalsRoot`
available to the EVM/application layer.

#### P2P

During sync, we expect the withdrawals list in the block body to be empty (OP stack does not make
use of the withdrawals list) and hence the hash of the withdrawals list to be the MPT root of an empty list.
When verifying the header chain using the final header that is synced, the header timesetamp is used to
determine whether Isthmus is active at the said block. If it is, we expect that the header `withdrawalsRoot`
MPT hash can be any non-null value (since it is expected to contain the `L2ToL1MessagePasser`'s storage root).

#### Backwards Compatibility Considerations

Beginning at Canyon (which includes Shanghai hardfork support) and prior to Isthmus activation,
the `withdrawalsRoot` field is set to the MPT root of an empty withdrawals list. This is the
same root as an empty storage root. The withdrawals are captured in the L2 state, however
they are not reflected in the `withdrawalsRoot`. Hence, prior to Isthmus activation,
even if a `withdrawalsRoot` is present and a MPT root is present in the header, it should not be used.
Any implementation that calculates output root should be careful not to use the header `withdrawalsRoot`.

After Isthmus activation, if there was never any withdrawal contract storage, a MPT root of an empty list
can be set as the `withdrawalsRoot`

#### Forwards Compatibility Considerations

As it stands, the `withdrawalsRoot` field is unused within the OP Stack's header consensus format, and will never be
used for other reasons that are currently planned. Setting this value to the account storage root of the withdrawal
directly fits with the OP Stack, and makes use of the existing field in the L1 header consensus format.

#### Client Implementation Considerations

Various EL clients store historical state of accounts differently. If, as a contrived case, an OP Stack chain did not have
an outbound withdrawal for a long period of time, the node may not have access to the account storage root of the
[`L2ToL1MessagePasser`][l2-to-l1-mp]. In this case, the client would be unable to keep consensus. However, most modern
clients are able to at the very least reconstruct the account storage root at a given block on the fly if it does not
directly store this information.

##### Transaction Simulation

In response to RPC methods like `eth_simulateV1` that allow simulation of arbitrary transactions within one or more blocks,
an empty withdrawals root should be included in the header of a block that consists of such simulated transactions. The same
is applicable for scenarios where the actual withdrawals root value is not readily available.

## Deposit Requests

[EIP-6110] shifts deposit to the execution layer, introducing a new [EIP-7685] deposit request of type
`DEPOSIT_REQUEST_TYPE`. Deposit requests then appear in the [EIP-7685] requests list. The OP Stack needs to ignore these
requests. Requests generation must be modified to exclude [EIP-6110] deposit requests. Note that since the [EIP-6110]
request type did _not_ exist prior to Pectra on L1 and the Isthmus hardfork on L2, no activation time is needed since these
deposit type requests may always be excluded.

[EIP-6110]: https://eips.ethereum.org/EIPS/eip-6110
[EIP-7685]: https://eips.ethereum.org/EIPS/eip-7685

## Block Body Withdrawals List

Withdrawals list in the block body is encoded as an empty RLP list.

## EVM Changes

Similar to the `bn256Pairing` precompile in the [granite hardfork](../granite/exec-engine.md),
[EIP-2537](https://eips.ethereum.org/EIPS/eip-2537) introduces a BLS
precompile that short-circuits depending on input size in the EVM.

The input size limits of the BLS precompile contracts are listed below:

- G1 multiple-scalar-multiply: `input_size <= 513760 bytes`
- G2 multiple-scalar-multiply: `input_size <= 488448 bytes`
- Pairing check: `input_size <= 235008 bytes`

The rest of the BLS precompiles are fixed-size operations which have a fixed gas cost.

All of the BLS precompiles should be [accelerated](../../fault-proof/index.md#precompile-accelerators) in fault proof
programs so they call out to the L1 instead of calculating the result inside the program.

## Block Sealing

[EIP-7251](https://eips.ethereum.org/EIPS/eip-7251) introduces new request type `0x02`, the `CONSOLIDATION_REQUEST_TYPE`.
Typed request envelopes debut in Pectra [EIP-7685](https://eips.ethereum.org/EIPS/eip-7685). Execution layer requests
continue to be ignored in Isthmus, including those of new type `0x02`. Note, this does not need to activate after any
specific hardfork as this type does not exist pre-Pectra.

## Engine API Updates

### Update to `ExecutableData`

`ExecutableData` will contain an extra field for `withdrawalsRoot` after Isthmus hard fork.

### `engine_newPayloadV4` API

Post Isthmus, `engine_newPayloadV4` will be used with the additional `ExecutionPayload` attribute. This attribute
is omitted prior to Isthmus.
