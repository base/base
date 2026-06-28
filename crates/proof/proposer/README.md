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
