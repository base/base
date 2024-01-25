# Plasma mode

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [DA Storage](#da-storage)
- [Input Commitment](#input-commitment)
- [Data Availability Challenge Contract](#data-availability-challenge-contract)
  - [Parameters](#parameters)
- [Derivation](#derivation)
- [Safety and Finality](#safety-and-finality)
- [Security Considerations](#security-considerations)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

Using [Plasma][vitalikblog] mode means switching to an offchain data availability provider.
The protocol guarantees availability with a challenge contract on L1 where users
can challenge the availability of input data used to derive the chain.
Challenging an input commitment means forcing providers to submit the input data on chain
to prove the data is available during a time period. We call the period during which any party
can challenge a commitment `challenge_window` and the period during which any party
can submit the input data to resolve the challenge `resolve_window`.
If a challenge is not resolved by the time `resolve_window` of L1 blocks have passed,
chain derivation must be reset, omitting the input data when rederiving therefore causing a reorg.

## DA Storage

Input data is uploaded to the storage layer via plain HTTP calls to the DA storage service.
This service is horizontally scalable and concerned with replicating the data across prefered storage layers
such as IPFS or any S3 compatible storage. Input data is content addressed by its hash in the request url
so responses are easily cachable.

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
    GET /get/<hex_encoded_commitment>
  
  Response:
    200 OK
    Content-Type: application/octet-stream
    Body: <preimage_bytes>
  ```

## Input Commitment

The batching and compression of input data remain unchanged. When a batch is ready
to be submitted to the inbox address, the data is uploaded to the DA storage layer instead, and a
commitment (keccak256 hash) is submitted as the bacher inbox transaction call data.
The batcher SHOULD not submit a commitment onchain unless input data was successfully stored on the service.

## Data Availability Challenge Contract

### Parameters

| Variable                      | Description                                                                 |
| ----------------------------- | --------------------------------------------------------------------------- |
| challengeWindow:  uint256 | L1 blocks period a commitment MAY be challenged after it's included onchain |
| resolveWindow:  uint256 | L1 blocks period input data SHOULD be submitted onchain after a challenge   |
| bondSize:   uint256 | Bond amount in Wei posted by the challenger so that bondSize >= resolveCost |
| resolverRefundFactor: uint256 | Factor defining the portion of the resolving cost refunded to the resolver  |

Data availability is guaranteed via a permissionless challenge contract on the L1 chain.
Users have a set number of L1 blocks (`challengeWindow`) during which they are able to call
the `challenge` method of the contract with the following inputs:

```js
function challenge(
    uint256 challengedBlockNumber,
    bytes32 commitmentType,
    bytes commitment
) external payable
```

- The L1 block number in which it was included.
- A Commitment type (i.e. keccak256("keccak256"))
- Commitment bytes

Users with access to the input data then have another window of L1 blocks (`resolveWindow`)
during which they can submit it as calldata to the chain by calling the `resolve` method of the contract.
If the data is not included onchain by the time the resolve window is elapsed, derivation of the L2 canonical chain
will omit the input data, hence any transaction included in the input data will be dropped
causing a reorg of any L2 chain that previously had included that transaction.

```js
function resolve(
    uint256 challengedBlockNumber,
    bytes32 commitmentType,
    bytes commitment,
    bytes calldata preImage
) external
```

In order to challenge a commitment, users deposit a bond amount where `bond >= resolve_tx_gas_cost`.
If the gas cost of resolving the challenge was lower than the bond, the difference is reimbursed to the challenger
and the rest of the bond is burnt. If the challenge is not resolved in time and expired,
the bond is returned and can be withdrawn by the challenger or used to challenge another commitment.
`bondSize` can be updated by the contract owner similar to [SystemConfig](specs/system-config.mg) variables.
See [Security Considerations](#security-considerations) for more details on bond management.

The state of all challenges can be read from the contract state or by syncing contract events.
`challengeWindow` and `resolveWindow` are constant values that currently cannot be changed
unless the contract is upgraded. A dynamic window mechanism may be explored in the future.

The contract is deployed behind upgradable proxy so the address can be hardcoded in the rollup config
file and does not need to change. A future upgrade can support custom resolver functions to be chosen
dynamically when a user calls the resolve function.

## Derivation

Input data is retrieved during derivation of L2 blocks. The changes to the derivation pipeline
when using the alt DA source are limited to swapping out the L1 based `DataAvailabilitySource` step
in the pipeline with a module that enables pulling the data from the offchain DA source.

Similarly to L1 based DA, for each L1 block we open a calldata source to retrieve the input commitments
from the transactions and use each commitment with its l1 origin block number to resolve
the input data from the storage service.

In addition, we filter events from the DA Challenge contract included in the block
and sync a local state of challenged input commitments. As the derivation pipeline steps through
`challenge_window + resolve_window` amount of L1 blocks, any challenge marked as active
becomes expired causing a reset of the derivation pipeline.

```js
// The status enum of a DA Challenge event
enum ChallengeStatus {
    Uninitialized,
    Active,
    Resolved,
    Expired
}

// DA Challenge event filtered
event ChallengeStatusChanged(
  bytes32 indexed challengedHash, uint256 indexed challengedBlockNumber, ChallengeStatus status
);

```

Derivation can either be driven by new L1 blocks or restarted (reset) from the L1 origin of the last
L2 safe head known by the execution engine.

When stepping through new L1 origin blocks, input data is loaded from the DA storage service.
If the service responds with a 404 (not found) error, derivation stalls and the DA Manager
starts a detached traversal of new L1 blocks until:

- An `Active` challenge event is found, detached L1 traversal continues:
  - A `Resolved` challenge event is found: derivation continues with the input data submitted onchain.
  - A high enough L1 block is reached such as `latest_l1 - pipeline_l1_origin = resolve_window_size`:
    input data is skipped from the derivation output.
- Data is never challenged, derivation returns critical error.

When derivation is reset, the pipeline steps through previously seen L1 origins so that
`block_start = latest_l1_block - (challenge_window_size + resolve_window_size + sequencer_window_size)`.
In that case, the DA manager has already synced the state of challenges during the range of L1 blocks traversed
so the pipeline can skip commitments with expired challenges and reorg the L2 chain
or load input data from the resolving transaction calldata.

## Safety and Finality

Similarly to rollup mode, the engine queue labels any new blocks derived from input data with a commitment
on the L1 chain as “safe”. Although labeled as “safe”, the chain might still reorg in case of a faulty DA provider
and users must use the “finalized” label for a guarantee that their state cannot revert.

With Plasma mode on, the engine queue does not receive finality signals from the L1 RPC
but from the DA manager that keeps track of challenges. The engine queue will maintain a longer buffer
of L2 blocks waiting for the DA windows to expire in order to be finalized.

## Security Considerations

The Data Availability Challenge contract mitigates DoS vulnerability with a payable bond requirement making
challenging the availability of a commitment at least as expensive as submitting the data onchain to resolve
the challenge.
In addition, the reward is not net positive for the [fisherman](https://arxiv.org/abs/1809.09044)
who forced the release of data by challenging thus preventing money pump vulnerability
while still making challenging affordable to altruistic fishermen and users who desire to pay
to guarrantee data availability on L1.
Lastly, if needed a `resolver_refund_factor` can be dialed up such as `resolver_refund_factor * resolving_cost`
is refunded to the resolver (where `0 <= refund_factor <= 1`) while the rest of the bond is burnt.


[vitalikblog]: https://vitalik.eth.limo/general/2023/11/14/neoplasma.html
