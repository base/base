# `base-proposer`

TEE-based output proposer for Base.

## Architecture

The proposer reads L2, rollup, and L1 state, requests a TEE-signed proposal,
verifies the output root locally, then submits through
`DisputeGameFactory.createWithInitData()` for onchain verification.

### Game Tracking and Parent Selection

Each dispute game references a parent game via `parent_address` in the factory.
At the top of every tick the proposer recovers its parent from chain.

`ProofRecovery::recover_latest_state()` reads the anchor root and anchor game
from `AnchorStateRegistry` in one snapshot, then walks forward from the anchor.
For each next proposal block it computes the expected root claim and extra data
from canonical rollup output roots and looks the game up with
`DisputeGameFactory.games()` for the configured `game_type`:

- If the game exists, it becomes the new parent and the walk continues.
- The walk stops at the first missing game, at a block past the finalized L2
  head, or at a block the rollup node does not have yet.
- If the anchor has no game, `AnchorStateRegistry` is used as the parent.
- If recovery fails, skip the tick and retry on the next one.

The last walk result is cached with the factory `game_count`. The cache is
reused while the count is unchanged and the finalized head has not reached the
next proposal block; a higher count resumes the walk from the cached tip, and a
lower count or an anchor past the cached tip restarts from the anchor.

Because state is recovered from chain, the proposer chains off games created
by any proposer and handles `GameAlreadyExists` without special recovery logic.
