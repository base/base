# ZK Prover

The ZK prover is an offchain service that uses SP1 programs to produce permissionless proofs for
checkpoint proposals and disputes. A proving service accepts block-range requests, persists proof
state, submits work to SP1 proving infrastructure, and returns receipts that callers can submit to
`AggregateVerifier`.

The ZK path is permissionless: any operator with canonical L1 and L2 RPC access, a configured SP1
backend, and an L1 transaction signer can request proofs and submit valid proof material onchain.

## Responsibilities

A conforming ZK prover stack performs the following work:

1. Accept proving requests for L2 block ranges.
2. Generate witness input from canonical L1, L2, and beacon RPCs.
3. Prove the range program with SP1.
4. For Groth16 requests, aggregate the completed range proof into an onchain-verifiable SNARK.
5. Persist proof request and backend session state so work can recover across process restarts.
6. Expose proof status and receipt retrieval over gRPC.
7. Encode receipts in the format expected by challengers, proposers, and `ZKVerifier`.

The ZK prover does not decide whether a game is valid. Proposers and challengers choose the range to
prove, recompute canonical roots themselves, and recheck game state before submitting proof material
onchain.

## Proving Service API

The proving service exposes:

```text
ProveBlock(ProveBlockRequest) -> ProveBlockResponse
GetProof(GetProofRequest) -> GetProofResponse
```

`ProveBlock` enqueues a proof request and returns a `session_id`. `GetProof` returns the current
status and, once complete, the requested receipt bytes.

### ProveBlock Request

`ProveBlockRequest` contains:

| Field                       | Meaning                                                                                                              |
| --------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `start_block_number`        | L2 block whose output root is the trusted starting state for the range.                                              |
| `number_of_blocks_to_prove` | Number of L2 blocks to prove after `start_block_number`.                                                             |
| `sequence_window`           | Optional L1 block lookahead used when deriving an L1 head for witness generation.                                    |
| `proof_type`                | `PROOF_TYPE_COMPRESSED` or `PROOF_TYPE_SNARK_GROTH16`.                                                               |
| `session_id`                | Optional caller-supplied UUID used for idempotent requests.                                                          |
| `prover_address`            | L1 address committed into the Groth16 journal so a proof cannot be replayed by another sender. Required for Groth16. |
| `l1_head`                   | Optional 32-byte hex L1 block hash used for witness generation.                                                      |

If `session_id` is supplied, duplicate requests with the same UUID return the existing session. This
lets challengers derive deterministic session IDs from `(game address, invalid checkpoint index)`
and retry safely across process restarts.

Callers supply `l1_head` when the proof journal must match a specific game context already
committed onchain (for example, dispute proofs against an existing game). When omitted, the service
derives an L1 head from the L2 block's L1 origin plus the request or service sequence window, which
is appropriate for fresh proposals where the caller has not yet committed to an L1 head.

`PROOF_TYPE_SNARK_GROTH16` requires `prover_address`: the aggregation program commits this address
into the journal digest, and `AggregateVerifier` rechecks the same digest before accepting the
proof, so a Groth16 receipt is bound to the L1 sender that requested it.

### Proof Types

The service supports two proof types:

| Proof type                    | Backend sessions | Result                                                                 |
| ----------------------------- | ---------------- | ---------------------------------------------------------------------- |
| `PROOF_TYPE_COMPRESSED`       | `STARK`          | A compressed SP1 range proof.                                          |
| `PROOF_TYPE_SNARK_GROTH16`    | `STARK`, `SNARK` | A range proof plus a Groth16 aggregation proof suitable for onchain use. |

For `PROOF_TYPE_SNARK_GROTH16`, the service first submits the range program as a compressed STARK
session. After that session completes, the service submits the aggregation program as a Groth16
SNARK session.

## Request Lifecycle

A proof request begins as `CREATED` once the request and outbox entry have been persisted. A
worker then claims the outbox task and moves the request to `PENDING` while it prepares and
submits backend work. After at least one backend session exists, the request is `RUNNING`. The
request becomes `SUCCEEDED` once all sessions required by the proof type complete and the receipt
bytes are stored, or `FAILED` if validation, witness generation, backend submission, backend
execution, receipt download, or retry recovery fails permanently.

Backend sessions track `RUNNING`, `COMPLETED`, or `FAILED` independently of the proof request. A
compressed request succeeds when all STARK sessions complete. A Groth16 request succeeds only
after both the STARK and SNARK sessions complete. Any failed session fails the parent request.

## Receipt Retrieval

`GetProofRequest` contains:

| Field          | Meaning                                                     |
| -------------- | ----------------------------------------------------------- |
| `session_id`   | UUID returned by `ProveBlock`.                              |
| `receipt_type` | Optional receipt selector. Defaults to `RECEIPT_TYPE_STARK`. |

The receipt selector can be:

| Receipt type                  | Response bytes                                                                            |
| ----------------------------- | ----------------------------------------------------------------------------------------- |
| `RECEIPT_TYPE_STARK`          | Serialized SP1 proof-with-public-values for the range proof.                              |
| `RECEIPT_TYPE_SNARK`          | Serialized SP1 proof-with-public-values for the aggregation proof.                        |
| `RECEIPT_TYPE_ON_CHAIN_SNARK` | Onchain proof bytes extracted from the stored SNARK receipt for the SP1 Groth16 verifier. |

`GetProof` returns empty receipt bytes while a request is `CREATED`, `PENDING`, or `RUNNING`.
Failed requests return `STATUS_FAILED` and the stored error message. A successful response always
carries non-empty receipt bytes; if the stored request is `Succeeded` but the requested receipt
kind is absent, `GetProof` returns gRPC `NOT_FOUND` rather than an empty success.

Callers are responsible for wrapping returned receipt bytes in the `AggregateVerifier` proof format.
For challenge, nullification, and additional-proof submission, the caller prefixes the ZK proof-type
byte before the receipt. For game initialization, the caller also includes the L1 origin fields
required by `initializeWithInitData()`. See [Contracts](./contracts) for the verifier-side framing.

## Backend Modes

The proving service supports these backend modes:

| Mode      | Purpose                                                                 |
| --------- | ----------------------------------------------------------------------- |
| `mock`    | Produces fake receipts for local tests without witness generation.      |
| `cluster` | Submits work to a self-hosted SP1 cluster with Redis or S3 artifacts.   |
| `network` | Submits work to the SP1 Network with the configured fulfillment policy. |

The `cluster` and `network` backends share the same witness generation path; only submission,
polling, and artifact retrieval differ. The `mock` backend skips witness generation entirely.

## SP1 Range Program

The range program proves a Base L2 state transition over a contiguous block range. Its stdin
contains:

```text
rkyv(DefaultWitnessData)
intermediateRootInterval
```

The program reconstructs the preimage oracle and beacon blob provider from the witness, runs the
Ethereum DA witness executor, and commits a `BootInfoStruct`.

The committed boot info contains:

| Field                      | Meaning                                                         |
| -------------------------- | --------------------------------------------------------------- |
| `l2PreRoot`                | Output root for the trusted starting L2 block.                  |
| `l2PreBlockNumber`         | Starting L2 block number.                                       |
| `l2PostRoot`               | Output root after executing the requested range.                |
| `l2BlockNumber`            | Ending L2 block number.                                         |
| `l1Head`                   | L1 block hash used for derivation data.                         |
| `rollupConfigHash`         | Hash of the rollup configuration used during execution.         |
| `intermediateRoots`        | Ordered output roots sampled every intermediate-root interval.  |

The final intermediate root must correspond to the ending L2 block for the range being proven.

## SP1 Aggregation Program

The aggregation program turns completed range proofs into the journal digest used by onchain
verification. Its inputs are:

```text
AggregationInputs              (sp1_zkvm::io::read)
L1 headers (CBOR-encoded)      (sp1_zkvm::io::read_vec)
compressed range proofs        (SP1 proof-input channel)
```

The compressed range proofs are passed via SP1's proof-input mechanism, not via plain stdin bytes,
and are verified inside the program with `sp1_lib::verify::verify_sp1_proof`.

`AggregationInputs` contains the range boot infos, the latest L1 checkpoint head, the range-program
verification key, and the prover address.

The aggregation program verifies:

1. At least one range boot info is present.
2. Adjacent range boot infos are sequential:

   ```text
   previous.l2PostRoot == next.l2PreRoot
   previous.l2BlockNumber == next.l2PreBlockNumber
   ```

3. Every range uses the same `rollupConfigHash`.
4. Every compressed range proof verifies against the supplied range verification key.
5. The provided L1 headers form a linked chain ending at `latest_l1_checkpoint_head`.
6. Every range `l1Head` appears in that header chain.

The program then flattens all intermediate roots and builds one aggregate output:

```text
proverAddress
l1Head
l2PreRoot
startingL2SequenceNumber
l2PostRoot
endingL2SequenceNumber
intermediateRoots
rollupConfigHash
imageHash
```

`imageHash` is the range-program verification key commitment. The aggregation program commits:

```text
keccak256(abi.encodePacked(AggregationOutputs))
```

This digest matches the journal hash assembled by `AggregateVerifier` for ZK proof verification. In
[Contracts](./contracts) terminology, `imageHash` is `ZK_RANGE_HASH`, and the aggregation
verification key configured on `ZKVerifier` is `ZK_AGGREGATE_HASH`.

## ELF Reproducibility

SP1 ELF binaries are built on demand and are not committed. The repository pins expected ELF
SHA-256 hashes in `crates/proof/succinct/elf/manifest.toml`. A code change that changes either SP1
program must rebuild the ELFs and update `manifest.toml` in the same change.

The range verification key commitment (`ZK_RANGE_HASH`) and aggregation verification key hash
(`ZK_AGGREGATE_HASH`) are onchain security parameters. Operators must deploy or configure verifier
contracts with values derived from the same ELFs used by the proving service.

## Retry Behavior

The service retries transient conditions without changing the logical proof request:

| Condition                                          | Required behavior                                                              |
| -------------------------------------------------- | ------------------------------------------------------------------------------ |
| Outbox task already claimed                        | Skip the duplicate worker.                                                     |
| Stuck `PENDING` request without an active session  | Reset to `CREATED` with a new outbox entry until the retry limit is exhausted. |
| Backend status polling error                       | Leave the request `RUNNING` and retry on a later poll.                         |
| Proof artifact unavailable after backend success   | Leave the session `RUNNING` or retry download on a later poll.                 |
| Backend reports failed or unfulfillable work       | Mark the session and proof request `FAILED`.                                   |
| Groth16 stage-two submission fails after STARK     | Mark the proof request `FAILED`.                                               |

Callers should treat `FAILED` as terminal for that stored request. If the proof is still needed, the
caller should submit or retry the same logical request using its deterministic `session_id`.

## Service Lifecycle

At startup, the proving service:

1. Connects to Postgres.
2. Optionally starts rate-limited local proxies for L1, L2, and beacon RPCs.
3. Loads rollup configuration from the rollup RPC.
4. Computes the range and aggregation proving and verifying keys.
5. Initializes the configured backend.
6. Starts the outbox processor.
7. Starts the status poller.
8. Starts the gRPC server and reflection service.

The outbox processor turns persisted requests into backend sessions. The status poller syncs running
sessions, downloads receipts, triggers Groth16 stage two when needed, and retries or fails stuck
requests.

## Operator Inputs

A ZK prover service needs:

- L1 execution RPC endpoint.
- L1 beacon RPC endpoint.
- L2 execution RPC endpoint.
- Rollup RPC endpoint.
- Postgres connection settings.
- SP1 backend configuration.
- Artifact storage configuration for cluster mode.
- Poll intervals, stuck-request timeout, and retry limits.
- Metrics and logging configuration.

Network mode additionally needs an SP1 Network signer or KMS requester configuration. Cluster mode
additionally needs an SP1 cluster endpoint and exactly one artifact storage backend.

## Onchain Expectations

ZK proof bytes are submitted to `AggregateVerifier` as proof type `ZK`. The game assembles the
expected journal from the proposal or dispute context and calls `ZKVerifier.verify()` with the
configured aggregation verification key.

A valid Groth16 receipt proves that the aggregation program committed the expected journal digest.
It does not replace caller-side state checks. Proposers and challengers must still recompute
canonical roots and recheck game state before submitting proof material.

## Safety Requirements

A ZK prover implementation must preserve these safety properties:

- Use the caller-provided `l1_head` when present, so dispute proofs match the game context stored
  onchain.
- Require `prover_address` for Groth16 proofs, because it is committed into the aggregation journal.
- Keep request creation idempotent for deterministic `session_id` values.
- Do not return onchain SNARK bytes unless the stored SNARK receipt deserializes successfully.
- Persist backend session metadata before relying on asynchronous backend completion.
- Pin ELF hashes so verification keys and onchain configuration do not silently drift.
- Treat unavailable RPC data, backend polling failures, and artifact download failures as retryable
  service conditions rather than proof validity results.
