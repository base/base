# Proposer

The proposer is an offchain service that turns canonical L2 checkpoint ranges into
`AggregateVerifier` games on L1. It selects the next checkpoint from the latest onchain parent
state, obtains a TEE proof for that range, validates the proof against canonical L2 state, and
creates the next dispute game through `DisputeGameFactory`.

The production proposer is controlled by its configured L1 transaction signer. Its output is still
self-validating: each game is uniquely identified by the game type, claimed output root, parent,
L2 block number, and intermediate output roots, and the proof can be checked by the onchain verifier
and by independent challengers.

## Responsibilities

A conforming proposer performs the following work:

1. Read the active `AggregateVerifier` implementation and proposal parameters from L1.
2. Recover the latest onchain parent state from `AnchorStateRegistry` and `DisputeGameFactory`.
3. Select the next checkpoint block that is no later than the chosen safe head.
4. Build a `prover_prove` request for the checkpoint range.
5. Accept only TEE proof results for proposal creation.
6. Revalidate the aggregate output root and all intermediate roots against canonical L2 state
   immediately before L1 submission.
7. Optionally pre-check the TEE signer against `TEEProverRegistry`.
8. Submit `DisputeGameFactory.createWithInitData()` with the required bond.
9. Retry transient proof, RPC, and transaction failures without creating out-of-order games.

The proposer does not challenge games, resolve games, claim bonds, or decide withdrawal finality.
Those responsibilities belong to the challenger and proof contracts.

## Startup Configuration

At startup, the proposer connects to:

- an L1 execution RPC for contract reads and transaction submission
- an L2 execution RPC for agreed L2 block headers
- a rollup RPC for sync status and output roots
- a prover RPC that implements `prover_prove`
- `AnchorStateRegistry`
- `DisputeGameFactory`
- an optional `TEEProverRegistry`

The proposer reads the game implementation address from:

```text
DisputeGameFactory.gameImpls(gameType)
```

The implementation address must be non-zero. The proposer then reads:

```text
AggregateVerifier.BLOCK_INTERVAL()
AggregateVerifier.INTERMEDIATE_BLOCK_INTERVAL()
DisputeGameFactory.initBonds(gameType)
```

`BLOCK_INTERVAL` must be at least `2`, `INTERMEDIATE_BLOCK_INTERVAL` must be non-zero, and
`BLOCK_INTERVAL % INTERMEDIATE_BLOCK_INTERVAL` must be `0`. The number of intermediate roots in a
proposal is:

```text
BLOCK_INTERVAL / INTERMEDIATE_BLOCK_INTERVAL
```

The proposer defaults to finalized L2 state. If explicitly configured to allow non-finalized
proposals, it may use the rollup node's safe L2 state instead.

## Parent Recovery

The proposer recovers the latest onchain parent state from L1 before planning new work. The parent
state is:

```text
parentAddress
parentOutputRoot
parentL2BlockNumber
```

If no matching games exist, the parent is the anchor root from `AnchorStateRegistry`:

```text
parentAddress = AnchorStateRegistry address
parentOutputRoot = AnchorStateRegistry.getAnchorRoot().root
parentL2BlockNumber = AnchorStateRegistry.getAnchorRoot().l2BlockNumber
```

If games exist, the proposer performs a deterministic forward walk from the anchor root, or from a
cached recovered tip when the cache is still valid. At each step:

1. Compute:

   ```text
   expectedBlock = parentL2BlockNumber + BLOCK_INTERVAL
   ```

2. Fetch the canonical output root for every intermediate checkpoint:

   ```text
   parentL2BlockNumber + INTERMEDIATE_BLOCK_INTERVAL * i
   ```

   for `i` in `1..=BLOCK_INTERVAL / INTERMEDIATE_BLOCK_INTERVAL`.

3. Treat the final intermediate root as the canonical root claim for `expectedBlock`.
4. Encode `extraData` from `expectedBlock`, `parentAddress`, and the ordered intermediate roots.
5. Look up the expected game:

   ```text
   DisputeGameFactory.games(gameType, rootClaim, extraData)
   ```

6. If the lookup returns `address(0)`, stop. The current parent is the latest recovered state.
7. Otherwise, advance the parent to the returned game proxy and continue.

This recovery method does not scan factory indices for a "best" game. It uses the game's unique
factory key, so only the canonical next game for the recovered parent can advance the chain of
parents. A game with the wrong root, parent, L2 block number, or intermediate roots has a different
key and is ignored by parent recovery.

## Checkpoint Selection

After recovery, the next proposal target is:

```text
targetBlock = parentL2BlockNumber + BLOCK_INTERVAL
```

The proposer must not request or submit a proof for `targetBlock` unless:

```text
targetBlock <= safeHead
```

where `safeHead` is either:

- `finalized_l2.number`, by default
- `safe_l2.number`, only when non-finalized proposals are explicitly enabled

When parallel proving is enabled, the proposer may request proofs for multiple future checkpoint
targets, but L1 submissions remain strictly sequential. At most one proposal transaction is in
flight, and the next transaction is not submitted until all earlier checkpoint games are recovered
or confirmed.

## Proof Request

For a checkpoint range, the proposer builds a `ProofRequest` with:

| Field                         | Value                                                      |
| ----------------------------- | ---------------------------------------------------------- |
| `l1_head`                     | Hash of the latest L1 block at request construction time   |
| `l1_head_number`              | Number of the latest L1 block at request construction time |
| `agreed_l2_head_hash`         | L2 block hash at `parentL2BlockNumber`                     |
| `agreed_l2_output_root`       | Parent output root recovered from L1                       |
| `claimed_l2_output_root`      | Rollup RPC output root at `targetBlock`                    |
| `claimed_l2_block_number`     | `targetBlock`                                              |
| `proposer`                    | L1 address that will submit the proposal transaction       |
| `intermediate_block_interval` | `INTERMEDIATE_BLOCK_INTERVAL`                              |
| `image_hash`                  | Expected TEE image hash                                    |

The prover RPC method is:

```text
prover_prove(ProofRequest) -> ProofResult
```

The proposer accepts `ProofResult::Tee` for proposal creation. A ZK proof result is not valid input
for the current proposer path.

## TEE Proposal Journal

The TEE prover returns:

- an aggregate proposal for the full checkpoint range
- per-block proposals for the blocks in that range

The aggregate proposal contains:

```text
outputRoot
signature
l1OriginHash
l1OriginNumber
l2BlockNumber
prevOutputRoot
configHash
```

The TEE signature is over:

```text
keccak256(journal)
```

where `journal` is packed as:

```text
proposer(20)
|| l1OriginHash(32)
|| prevOutputRoot(32)
|| startingL2Block(8)
|| outputRoot(32)
|| endingL2Block(8)
|| intermediateRoots(32 * N)
|| configHash(32)
|| teeImageHash(32)
```

For aggregate proposals:

```text
startingL2Block = parentL2BlockNumber
endingL2Block = targetBlock
prevOutputRoot = parentOutputRoot
outputRoot = claimed root at targetBlock
```

The ordered `intermediateRoots` are sampled every `INTERMEDIATE_BLOCK_INTERVAL` blocks and include
the final target block root.

## Pre-Submission Validation

Immediately before submitting to L1, the proposer must re-check the proof against canonical L2
state:

1. Fetch the rollup output root at `targetBlock`.
2. Require it to equal the aggregate proposal's `outputRoot`.
3. Extract the intermediate roots from the per-block proposals.
4. Fetch the canonical output root for each intermediate checkpoint.
5. Require every proposed intermediate root to equal its canonical root.

If the aggregate root or any intermediate root no longer matches canonical state, the proposer
discards the pending work and restarts recovery. This protects against stale proof results after L1
or L2 reorgs.

When `TEEProverRegistry` is configured, the proposer should recover the TEE signer from the
aggregate proposal signature and call:

```text
TEEProverRegistry.isValidSigner(signer)
```

If the registry returns `false`, the proposer must not submit that proof. It should discard the
proof and request a new one. If the registry check itself fails because of an RPC or deployment
issue, the proposer may continue to submission and rely on the onchain verifier to enforce signer
validity.

## Game Creation

The proposer creates a game with:

```solidity
DisputeGameFactory.createWithInitData{value: initBond}(
    gameType,
    rootClaim,
    extraData,
    initData
)
```

where:

```text
rootClaim = aggregateProposal.outputRoot
```

`extraData` is packed, not ABI-encoded:

```text
l2BlockNumber(32) || parentAddress(20) || intermediateRoots(32 * N)
```

`l2BlockNumber` is encoded as a 32-byte big-endian integer. `parentAddress` is the recovered parent
game proxy address, or the `AnchorStateRegistry` address for the first game after the anchor.

`initData` is the TEE proof bytes for `AggregateVerifier.initializeWithInitData()`:

```text
proofType(1) || l1OriginHash(32) || l1OriginNumber(32) || signature(65)
```

For TEE proofs:

```text
proofType = 0
```

The ECDSA `v` value in the signature must be normalized to `27` or `28` before submission.

`initBond` is read from `DisputeGameFactory.initBonds(gameType)` at startup and is sent as the
transaction value. Nonce management, fee bumping, signing, and transaction resubmission are handled
by the L1 transaction manager.

## Duplicate Games

The factory key for a game is:

```text
gameType || rootClaim || extraData
```

If `createWithInitData()` reverts with `GameAlreadyExists`, the proposer treats the target as
already submitted. It refreshes recovery from L1 and continues from the recovered tip. This handles
the case where a previous transaction succeeded but the proposer did not observe the receipt, or
where another valid proposer submitted the same game first.

## Retry Behavior

The proposer retries transient failures on later ticks:

| Failure                               | Required behavior                                           |
| ------------------------------------- | ----------------------------------------------------------- |
| Recovery RPC or contract read failure | Skip the current tick and retry recovery on the next tick   |
| Proof request failure                 | Retry the target on a later tick                            |
| Repeated proof failure                | Reset pipeline state and recover from L1                    |
| L1 submission failure                 | Keep the proved result and retry submission on a later tick |
| L1 submission timeout                 | Treat as a submission failure and retry after recovery      |
| `GameAlreadyExists`                   | Treat as success, refresh recovery, and continue            |
| Canonical root mismatch               | Reset pipeline state and re-prove from recovered L1 state   |
| Invalid TEE signer                    | Discard the proof and request a new one                     |

The current implementation retries a single proof target up to three times before resetting pipeline
state. Proposal submission is bounded by a ten minute timeout.

## Admin Interface

The proposer may expose an optional JSON-RPC admin interface. When enabled, it provides:

| Method                  | Result                                  |
| ----------------------- | --------------------------------------- |
| `admin_startProposer`   | Starts the proving pipeline             |
| `admin_stopProposer`    | Stops the proving pipeline              |
| `admin_proposerRunning` | Returns whether the pipeline is running |

Starting an already running proposer and stopping a stopped proposer are errors.

## Dry Run Mode

In dry run mode, the proposer performs recovery, checkpoint selection, proof sourcing, and
pre-submission validation, but it does not submit L1 transactions. Instead, it logs the game that
would have been created.

Dry run mode is useful for validating prover and RPC behavior, but it does not advance the onchain
proposal chain.
