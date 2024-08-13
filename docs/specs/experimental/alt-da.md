# Alt-DA Mode

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [Input Commitment Submission](#input-commitment-submission)
- [DA Server](#da-server)
- [Data Availability Challenge Contract](#data-availability-challenge-contract)
  - [Parameters](#parameters)
- [Derivation](#derivation)
- [Fault Proof](#fault-proof)
  - [`l2-input <commitment>`](#l2-input-commitment)
  - [`l1-challenge-status <commitment> <blocknumber>`](#l1-challenge-status-commitment-blocknumber)
- [Safety and Finality](#safety-and-finality)
- [Security Considerations](#security-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

Note: Alt-DA Mode is a Beta feature of the MIT licensed OP Stack.
While it has received initial review from core contributors, it is still undergoing testing,
and may have bugs or other issues.

## Overview

Using [Alt-DA][vitalikblog] mode means switching to an offchain data availability provider.
The protocol guarantees availability with a challenge contract on L1 where users
can challenge the availability of input data used to derive the chain.
Challenging an input commitment means forcing providers to submit the input data on chain
to prove the data is available during a time period. We call the period during which any party
can challenge a commitment `challenge_window` and the period during which any party
can submit the input data to resolve the challenge `resolve_window`.
If a challenge is not resolved by the time `resolve_window` of L1 blocks have passed,
chain derivation must be reset, omitting the input data when rederiving therefore causing a reorg.

[vitalikblog]: https://vitalik.eth.limo/general/2023/11/14/neoplasma.html

## Input Commitment Submission

The [batching][batcher] and compression of input data remain unchanged. When a batch is ready
to be submitted to the inbox address, the data is uploaded to the DA storage layer instead, and a
commitment (specified below) is submitted as the batcher inbox transaction call data.

Commitment txdata introduces version `1` to the [transaction format][batchertx], in order to interpret
the txdata as a commitment during the l1 retrieval step of the derivation pipeline:

| `version_byte` | `tx_data`   |
| -------------- | -------------------- |
| 1              | `encoded_commitment` |

The `derivationVersion0` byte is still prefixed to the input data stored in the DA provider so the frames
can be decoded downstream.

Commitments are encoded as `commitment_type_byte ++ commitment_bytes`, where `commitment_bytes` depends
on the `commitment_type_byte` where [0, 128) are reserved for official implementations:

| `commitment_type` | `commitment`                    |
| ----------------- | ------------------------------- |
| 0                 | `keccak256(tx_payload)`         |
| 1                 | `da-service`                     |

The `da-service` commitment is as follows: `da_layer_byte ++ payload`.
The DA layer byte must be initially restricted to the range `[0, 127)`.
This specification will not apportion DA layer bytes, but different DA layers should coordinate to ensure that the
DA layer bytes do not conflict. DA Layers can do so in
[this discussion](https://github.com/ethereum-optimism/specs/discussions/135).
The payload is a bytestring which is up to the DA layer to specify.
The DA server should be able to parse the payload, find the data on the DA layer, and verify that the data returned
from the DA layer matches what was committed to in the payload.

The batcher SHOULD cap input payloads to the maximum L1 tx size or the input will be skipped
during derivation. See [derivation section](#derivation) for more details.

The batcher SHOULD NOT submit a commitment onchain unless input data was successfully stored on the service.
In addition, a DA provider storage service SHOULD return an error response if it is unable to properly
store the request payload so as to signal to the batcher to retry.
Input commitments submitted onchain without proper storage on the DA provider service are subject to
challenges if the input cannot be retrieved during the challenge window, as detailed in the following section.

[batcher]: ../protocol/derivation.md#batch-submission
[batchertx]: ../protocol/derivation.md#batcher-transaction-format

## DA Server

Input data is uploaded to the storage layer via plain HTTP calls to the DA server.

This service is responsible to interacting with the Data Availability Layer (DA layer).
The layer could be a content addressable storage layer like IPFS or any S3 compatible storage
or it could a specific DA focused blockchain.
Content addressed systems like S3 should use the first `put/<hex_encoded_commitment>`
because they can pre-compute the commitment.
Blockchain based DA layers should use `put` and then submit the returned commitment to L1.
Because commitments can include the block height or hash, the commitment cannot be computed prior to submitting
it to the DA Layer.

Any DA provider can implement the following endpoints to receive and serve input data:

- ```text
  Request:
    POST /put/<hex_encoded_commitment>
    Content-Type: application/octet-stream
    Body: <preimage_bytes>

  Response:
    200 OK
  ```

- ```text
  Request:
    POST /put
    Content-Type: application/octet-stream
    Body: <preimage_bytes>

  Response:
    200 OK
    Content-Type: application/octet-stream
    Body: <hex_encoded_commitment>
  ```

- ```text
  Request:
    GET /get/<hex_encoded_commitment>

  Response:
    200 OK
    Content-Type: application/octet-stream
    Body: <preimage_bytes>
  ```

## Data Availability Challenge Contract

### Parameters

| Constant | Type | Description     |
| -------- | ---- | ---------------- |
| fixedResolutionCost | `uint256` | Fixed gas cost of resolving a challenge, set to 72925 |
| variableResolutionCost | `uint256` | Upper limit gas cost per byte scaled by precision constant, set to 16640 |
| variableResolutionCostPrecision | `uint256` | Precision of the variable resolution cost, set to 1000 |

| Variable  | Type                     | Description                                                                 |
| --------- | ----------------- | --------------------------------------------------------------------------- |
| challengeWindow |  `uint256` | Number of L1 blocks whereby a commitment MAY be challenged after it's included onchain |
| resolveWindow |  `uint256` | Number of L1 blocks whereby input data SHOULD be submitted onchain after a challenge   |
| bondSize |   `uint256` | Bond amount in Wei posted by the challenger so that bondSize >= resolveCost |
| resolverRefundFactor | `uint256` | Factor defining the portion of the resolving cost refunded to the resolver  |

Data availability is guaranteed via a permissionless challenge contract on the L1 chain.
Users have a set number of L1 blocks (`challengeWindow`) during which they are able to call
the `challenge` method of the contract with the following inputs:

**Note:** Resolving Input Data through the Data Availability Challenge Contract is implemented
only for `type=0 (keccak)` commitments. This is because `type=1 (da-service)` commitments are designed to be
handled by a DA Server which is responsible for the mapping between commitment and input data.
Due to this "generic" handling nature, there is currently no on-chain mechanism to verify commitments.

```solidity
function challenge(uint256 challengedBlockNumber, bytes calldata challengedCommitment) external payable
```

- The L1 block number in which it was included.
- Versioned commitment bytes (i.e. `0 ++ keccak256(frame.. )`)

Users with access to the input data then have another window of L1 blocks (`resolveWindow`)
during which they can submit it as calldata to the chain by calling the `resolve` method of the contract.
If the data is not included onchain by the time the resolve window is elapsed, derivation of the L2 canonical chain
will reorg starting from this first block derived from the challenged input data to the last block derived from the
L1 block at which it expired. See more details about [Derivation](#derivation) in the following section.

```solidity
function resolve(uint256 challengedBlockNumber, bytes calldata challengedCommitment, bytes calldata resolveData) external
```

In order to challenge a commitment, users deposit a bond amount where `bond >= resolve_tx_gas_cost`.
If the gas cost of resolving the challenge was lower than the bond, the difference is reimbursed to the challenger where
`cost = (fixedResolutionCost + preImageLength * variableResolutionCost / variableResolutionCostPrecision) * gasPrice`
and the rest of the bond is burnt. If the challenge is not resolved in time and expired,
the bond is returned and can be withdrawn by the challenger or used to challenge another commitment.
`bondSize` can be updated by the contract owner similar to [SystemConfig](../protocol/system-config.md) variables.
See [Security Considerations](#security-considerations) for more details on bond management.

The state of all challenges can be read from the contract state or by syncing contract events.
`challengeWindow` and `resolveWindow` are constant values that currently cannot be changed
unless the contract is upgraded. A dynamic window mechanism may be explored in the future.

Any challenge with a properly encoded commitment, a bond amount associated with the challenger
address and a block number within the challenge window is accepted by the contract.
However, a challenge is only valid if the commitment and block number pairing map to a valid commitment
and its L1 origin block number during derivation. Challenges associated with an illegal commitment
or block number will be ignored during derivation and have no impact on the state of the chain.

The contract is deployed behind upgradable proxy so the address can be hardcoded in the rollup config
file and does not need to change. A future upgrade can add custom resolver functions to be chosen
dynamically when a user calls the resolve function to support other alt DA solutions.

## Derivation

Input data is retrieved during derivation of L2 blocks. The changes to the derivation pipeline
when using the alt DA source are limited to wrapping the L1 based `DataAvailabilitySource` step
in the pipeline with a module that enables pulling the data from the offchain DA source once
we've extracted the commitment from L1 DA.

Similarly to L1 based DA, for each L1 block we open a calldata source to retrieve the input commitments
from the transactions and use each commitment with its l1 origin block number to resolve
the input data from the storage service. To enable smooth transition between alt-da and rollup mode, any L1 data
retrieved from the batcher inbox that is not prefixed with `txDataVersion1` is forwarded downstream
to be parsed as input frames or skipped as invalid data.

In addition, we filter events from the DA Challenge contract included in the block
and sync a local state of challenged input commitments. As the derivation pipeline steps through
`challenge_window + resolve_window` amount of L1 blocks, any challenge marked as active
becomes expired causing a reset of the derivation pipeline.

```solidity
// The status enum of a DA Challenge event
enum ChallengeStatus {
    Uninitialized,
    Active,
    Resolved,
    Expired
}

// DA Challenge event filtered
event ChallengeStatusChanged(
  bytes indexed challengedCommitment, uint256 indexed challengedBlockNumber, ChallengeStatus status
);

```

Derivation can either be driven by new L1 blocks or restarted (reset) from the L1 origin of the last
L2 safe head known by the execution engine. The model here is of weak subjectivity whereby a new node
coming onto the network can connect to at least 1 honest peer and sync blocks in the `challenge_window`
to reconstruct the same state as the rest of the network. As with [EIP-4844][eip4844],
applications SHOULD take on the burden of storing historical data relevant to themselves beyond
the challenge window.

When stepping through new L1 origin blocks, input data is loaded from the DA storage service.
If the service responds with a 404 (not found) error, derivation stalls and the DA Manager
starts a detached traversal of new L1 blocks until:

- An `Active` challenge event is found, detached L1 traversal continues:
  - A `Resolved` challenge event is found: derivation continues with the input data submitted onchain.
  - A high enough L1 block is reached such as `latest_l1 - pipeline_l1_origin = resolve_window_size`:
    input data is skipped from the derivation output.
- Data is never challenged, derivation returns critical error.

When [derivation is reset][pipeline], the pipeline steps through previously seen L1 origins so that:
`sync_starting_point = latest_l1_block - (challenge_window_size + resolve_window_size + sequencer_window_size)`.
In that case, the DA manager has already synced the state of challenges during the range of L1 blocks traversed
so the pipeline can either skip commitments with expired challenges and reorg the L2 chain
or load input data from the resolving transaction calldata.
In addition, an expired challenge will reorg out `[r_start, r_end]` L2 blocks so that `r_start` is the first
block derived from the expired challenge's input and `r_end` the last L2 block derived before the pipeline
was reset.

Derivation MUST skip input data such as `input_data_size > MAX_L1_TX_SIZE` where `MAX_L1_TX_SIZE` is a consensus
constant of 130672 bytes when `commitment_type == 0`. Different DA layers can potentially resolve the challenge
without immediately loading all of the data onto L1 in a single transaction & therefore are exempt from this limit.
In theory `MAX_L1_TX_SIZE` could be increased up to `(tx_gas_limit - fixed_resolution_cost) / dynamic_resolution_cost`
based on the cost of resolving challenges in the contract implementation however to make challenging accessible
it is capped based on geth's txMaxSize.
130672 is chosen as 131072 - 400. Geth rejects transactions from the mempool with a total serialized size over 131072.
400 bytes are allocated as overhead (signature, to address, metadata).

[pipeline]: ../protocol/derivation.md#resetting-the-pipeline
[eip4844]: https://eips.ethereum.org/EIPS/eip-4844

## Fault Proof

The derivation pipeline is integrated with [fault proofs][faultproofs] by adding additional hint types to the
preimage oracle in order to query the input data from the DA provider as well as onchain challenge status.

### `l2-input <commitment>`

The input data stored on the DA storage for the given `<commitment>`.

### `l1-challenge-status <commitment> <blocknumber>`

The status of the challenge for the given `<commitment>` at the given `<blocknumber>` on the L1
DataAvailabilityChallenge contract.

[faultproofs]: ../fault-proof/index.md

## Safety and Finality

Similarly to rollup mode, the engine queue labels any new blocks derived from input data with a commitment
on the L1 chain as “safe”. Although labeled as “safe”, the chain might still reorg in case of a faulty DA provider
and users must use the “finalized” label for a guarantee that their state cannot revert.

With Alt-DA mode on, the engine queue does receive finality signals from the L1 RPC AND
from the DA manager that keeps track of challenges.
The DA manager maintains an internal state of all the input commitments in the current `challengeWindow`
as they are validated by the derivation pipeline.

The L2 chain can be marked as finalized when the L1 block in which commitments can no longer be invalidated
becomes finalized. Without Alt-DA Mode, L2 blocks are safe when the batch is on L1. Plasma commitments are
only "optimistically safe" when the commitment is found in a L1 block. Commitments then become safe
when that commitment can no longer be invalidated. Then finalization can proceed as normal: when the L1 block
that an L2 block is derived from becomes finalized, the L2 block can be marked as finalized.

The engine queue will maintain a longer buffer of L2 blocks waiting for the DA window to expire
and the L1 block with the commitment to be finalized in order to signal finality.

## Security Considerations

The Data Availability Challenge contract mitigates DoS vulnerability with a payable bond requirement making
challenging the availability of a commitment at least as expensive as submitting the data onchain to resolve
the challenge.
In addition, the reward is not net positive for the [fisherman](https://arxiv.org/abs/1809.09044)
who forced the release of data by challenging thus preventing money pump vulnerability
while still making challenging affordable to altruistic fishermen and users who desire to pay
to guarantee data availability on L1.
Lastly, if needed a `resolver_refund_factor` can be dialed up such as `resolver_refund_factor * resolving_cost`
is refunded to the resolver (where `0 <= refund_factor <= 1`) while the rest of the bond is burnt.
