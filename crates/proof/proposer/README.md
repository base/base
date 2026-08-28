# `base-proposer`

TEE-based output proposer for Base.

## Architecture

The proposer reads L2, rollup, and L1 state, requests a TEE-signed proposal,
verifies the output root locally, then submits through
`DisputeGameFactory.createWithInitData()` for onchain verification.

### Game Tracking and Parent Selection

Each dispute game references a parent game via `parent_address` in the factory.
The proposer carries no cached parent state; it loads the latest game from chain
at the top of every tick.

`recover_latest_state()` walks backwards through the `DisputeGameFactory` (up to
`MAX_FACTORY_SCAN_LOOKBACK` entries, default 5000) to find the most recent game
matching the configured `game_type`:

- If a matching game exists, use it as the parent.
- If none exists, use `AnchorStateRegistry`.
- If recovery fails, skip the tick and retry on the next one.

Because state is always loaded from chain, the proposer chains off games created
by any proposer, handles `GameAlreadyExists` without special recovery logic, and
cannot enter stale-state livelocks.

### Artifact Pinning

The proposer stamps every TEE proof request with the enclave image hash the
target verifier accepts, so the prover service can route the job to a worker
running that exact enclave image.

`--tee-image-hash` / `BASE_PROPOSER_TEE_IMAGE_HASH` is **required** and has no
default — the proposer will not start without it. It must be set to the
`TEE_IMAGE_HASH()` of the currently deployed `AggregateVerifier` implementation.

The value also feeds the TEE proof session ID, so changing it changes every
derived session ID. During a proof-system upgrade the ordering matters:

1. Pause the proposer.
2. Point the factory at the new `AggregateVerifier` implementation.
3. Restart the proposer with `BASE_PROPOSER_TEE_IMAGE_HASH` set to the new
   implementation's `TEE_IMAGE_HASH`.

Restarting with a stale hash creates jobs that no current worker will claim.
