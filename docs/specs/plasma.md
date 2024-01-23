# Plasma mode

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
- [DA Storage](#da-storage)
- [Input Commitment](#input-commitment)
- [Data Availability Challenge Contract](#data-availability-challenge-contract)
- [Derivation](#derivation)
- [Safety and finality](#safety-and-finality)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

Using Plasma mode means switching to an offchain data availability provider.
The protocol guarrantees availability with a challenge contract on L1 where users
can challenge the availability of input data used to derive the chain.
Challenging an input commitment means forcing providers to submit the input data on chain
to prove the data is available during a time period. We call the period during which any party
can challenge a commitment `challenge_window` and the period during which any party
can submit the input data to resolve the challenge `resolve_window`.
If a challenge is not resolved by the time `resolve_window` of L1 blocks have passed,
chain derivation must be reset, omitting the input data when rederiving therefore causing a reorg.

## DA Storage

Input data is uploaded to the storage layer via plain HTTP calls to a stateless DA storage service.
This service is horizontally scalable and concerned with replicating the data across prefered storage layers
such as IPFS or any S3 compatible storage. Input data is content addressed by its hash in the request url
so responses are easily cachable.

Any DA provider can implement the following endpoints to receive and serve input data:

- `POST /put/<hex_encoded_commitment> -H "Content-Type: application/octet-stream"`
- `GET /get/<hex_encoded_commitment> -H "Content-Type: application/octet-stream"`

## Input Commitment

The batching and compression of input data remain unchanged. When a batch is ready
to be submitted to the inbox address, the data is uploaded to the DA storage layer instead, and a
commitment (keccak256 hash) is submitted as the bacher inbox transaction call data.
The batcher will not submit a commitment onchain unless input data was successfully stored on the service.

## Data Availability Challenge Contract

Data availability is guaranteed via a permissionless challenge contract on the L1 chain.
Users have a set number of L1 blocks (AKA `challenge_window`) during which they are able to call
the `challenge` method of the contract with the following inputs:

- A Commitment type (i.e. keccak256)
- Commitment bytes
- The L1 block number in which it was included.

Users with access to the input data then have another window of L1 blocks (AKA `resolve_window`)
during which they can submit it as calldata to the chain by calling the `resolve` method of the contract.
If the data is not included onchain by the time the resolve window is elapsed, derivation of the L2 canonical chain
will omit the input data, hence any transaction included in the input data will be dropped
causing a reorg of any L2 chain that previously had included that transaction.

In order to challenge a commitment, users must deposit a bond amount where `bond >= resolve_tx_gas_cost`.
If the gas cost of resolving the challenge was lower than the bond, the difference is reimbursed to the challenger
and the rest of the bond is burnt. If the challenge is not resolved in time and expired,
the bond is returned and can be withdrawn by the challenger or used to challenge another commitment.

The state of all challenges can be read from the contract state or by syncing contract events.
`challenge_window` and `resolve_window` are constant values that currently cannot be changed
unless the contract is upgraded. A dynamic window mechanism may be explored in the future.

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

## Safety and finality

Similarly to rollup mode, the engine queue labels any new blocks derived from input data with a commitment
on the L1 chain as “safe”. Although labeled as “safe”, the chain might still reorg in case of a faulty DA provider
and users must use the “finalized” label for a guarantee that their state cannot revert.

With Plasma mode on, the engine queue does not receive finality signals from the L1 RPC
but from the DA manager that keeps track of challenges. The engine queue will maintain a longer buffer
of L2 blocks waiting for the DA windows to expire in order to be finalized.
