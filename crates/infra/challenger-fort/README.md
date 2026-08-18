# `base-challenger-fort`

Observer for the live Challenger after deploy.

Talks to the live L1 factory and the live Challenger's Prometheus endpoint.
It never forks, never plants a game, and never probes `/healthz` or `/readyz`.
If the factory has no in-progress game of the configured type, FORT exits 0 —
a Proposer stall is not a Challenger fail.

## What it asserts

Lookback over live factory indices (`CHALLENGER_FORT_GAME_LOOKBACK`, default
50), same `game_type`, `InProgress`:

1. **No games.** Log skip and exit 0.
2. **Good game** (intermediate roots match L2 via `OutputValidator`). Pass
   requires the Challenger to have scanned it (`scan_head` at or past the
   factory index, or `games_scanned_total > 0` covering it) and left it
   undisputed. If every observed game is good, `games_invalid_total`,
   `nullify_tx_submitted_total`, and `challenge_tx_submitted_total` stay flat
   across the window.
3. **Bad game** (roots do not match L2). Pass requires it was scanned and
   disputed on-chain within the window, matching the classifier path:
   - Path 1 (TEE only): `teeProver == 0` or (`zkProver != 0` and
     `counteredIndex != 0`)
   - Path 3 (ZK only): `zkProver == 0`
   - Path 4 (TEE+ZK, `counteredIndex == 0`): TEE cleared
   - Path 2 (TEE+ZK, `counteredIndex > 0`): if the original root at the
     challenged index is correct, ZK is nullified; if that root is wrong,
     skip (legitimate challenge)
4. **L2 prune / `BlockNotAvailable`.** Skip that game; it is not a Challenger
   fail.

The window is `CHALLENGER_FORT_WINDOW` (default 10m). FORT polls until the
window elapses or the pass conditions hold — it does not fail early just
because the Challenger has not scanned yet.

## Required environment

`BASE_CHALLENGER_*` is shared with the live Challenger. `CHALLENGER_FORT_*`
belongs to the observer.

| Variable | Required | Purpose |
|----------|----------|---------|
| `BASE_CHALLENGER_L1_ETH_RPC` | Yes | Live L1 RPC |
| `BASE_CHALLENGER_L2_ETH_RPC` | Yes | Live L2 RPC for canonical output roots |
| `BASE_CHALLENGER_DISPUTE_GAME_FACTORY_ADDR` | Yes | `DisputeGameFactory` on L1 |
| `BASE_CHALLENGER_GAME_TYPE` | Yes | `AggregateVerifier` game type |
| `CHALLENGER_FORT_CHALLENGER_METRICS_URL` | No (default `http://base-challenger:7300/metrics`) | Live Challenger Prometheus endpoint |
| `CHALLENGER_FORT_GAME_LOOKBACK` | No (default `50`) | Newest factory indices to inspect |
| `CHALLENGER_FORT_WINDOW` | No (default `10m`) | Observation budget |
| `CHALLENGER_FORT_POLL_INTERVAL` | No (default `5s`) | Poll interval |

## Usage

```toml
[dependencies]
base-challenger-fort = { workspace = true }
```

```rust,ignore
use base_challenger_fort::ChallengerFort;

ChallengerFort::run().await?;
```

The `base-challenger-fort` binary wraps this for K8s Job execution. The Job
that consumes it lives in `protocols/base-proofs`.
