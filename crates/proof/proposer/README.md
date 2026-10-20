# `base-proposer`

TEE-based output proposer for Base.

## Architecture

The proposer reads L2, rollup, and L1 state, requests a TEE-signed proposal,
verifies the output root locally, then submits through
`DisputeGameFactory.createWithInitData()` for onchain verification.

### Game Tracking and Parent Selection

`recover_latest_state()` walks forward from the anchor root read from
`AnchorStateRegistry`. The parent starts as the anchor game, or the registry
address as a sentinel when no anchor game exists. Recovery resumes from a cached
tip when its block is at least the anchor block and factory `game_count` has not
decreased.

For each next expected proposal block, it builds the deterministic game key
from canonical roots fetched from the rollup node, then calls
`DisputeGameFactory.games(game_type, root_claim, extra_data)`. It follows matching
games regardless of proposer, stopping at the first missing game, a block the
rollup node does not have yet, or before exceeding `finalized_head`.

The pipeline caches `{game_count, state}` across ticks. Recovery can return the
usable cached state without walking when the count is unchanged and the next
proposal block exceeds `finalized_head`. Tick planning also skips recovery
before rereading the factory or anchor while the finalized head is below the
cached state's next proposal block. If recovery fails, the tick is skipped and
retried on the next one.
