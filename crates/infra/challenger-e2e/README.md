# `base-challenger-e2e`

Behavioural end-to-end test of the challenger.

Forks the target L1 into a pod-local Anvil, hands the fork to a real
`base-challenger` binary running alongside it, and asserts on what that
challenger does.

This crate currently covers the positive case: the challenger comes up against
the fork, scans it, and disputes nothing. The dispute paths are staged on top
of the same harness.

The challenger key is generated per run and never leaves the pod. It is the
only key on the fork that may dispute; the dispute paths add a second key that
signs their setup and nothing else.

## What it asserts

One process, one fork, two TEE-only in-progress games (newest-first, lookback
50, at least one intermediate root, all above the anchor game). The run bails if
fewer than two such games exist — they are the games the dispute paths will
corrupt, so a fork without them cannot exercise the challenger at all.

The anchor bound is not cosmetic: the scanner starts at one past the anchor
game's factory index, so a game at or before the anchor is one the challenger
will never look at, however invalid it is made.

1. **The challenger comes up.** `base_challenger_up` is 1 and at least one scan
   has completed.
2. **The challenger leaves valid games alone.** `games_invalid_total`,
   `nullify_tx_submitted_total` and `challenge_tx_submitted_total` must be zero
   outright on the first post-scan scrape, and must still be zero after
   `CHALLENGER_E2E_QUIET_WINDOW`. Nothing on the fork has been corrupted, so a
   challenger that disputes anything here is disputing a valid game.

   The baseline is checked absolutely rather than as a delta because
   `games_scanned_total` is incremented for the whole scanned range *before*
   any candidate is validated — a challenger that disputed during startup would
   otherwise be absorbed into the baseline and pass.

   Progress over the window is asserted on
   `validation_latency_seconds_count` minus `validation_errors_total`, not on
   `games_scanned_total`. The last counts attempted factory indices and
   advances even when every game query fails. The histogram is closer — it is
   only touched from inside the validator — but its latency is recorded from a
   drop guard, so it too counts attempts rather than successes. Subtracting the
   error counter, which the validator increments exactly once per failed call,
   leaves the games the challenger actually managed to check.

Validation errors below that threshold are reported rather than fatal — they
are usually the L2 RPC rather than the challenger.

## Required environment

`BASE_CHALLENGER_*` is shared with the challenger under test — both read the
same config-service mapping, so the driver forks exactly the L1 the challenger
is pointed at and talks to the same prover-service.

| Variable | Required | Purpose |
|----------|----------|---------|
| `BASE_CHALLENGER_L1_ETH_RPC` | Yes | L1 the fork is taken from; only ever read |
| `BASE_CHALLENGER_L2_ETH_RPC` | Yes | L2 archive RPC for canonical output roots |
| `BASE_CHALLENGER_ZK_RPC_URL` | Yes | Live prover-service JSON-RPC (not the fork) |
| `BASE_CHALLENGER_DISPUTE_GAME_FACTORY_ADDR` | Yes | `DisputeGameFactory` on L1 |
| `BASE_CHALLENGER_GAME_TYPE` | Yes | `AggregateVerifier` game type |
| `BASE_CHALLENGER_ANCHOR_STATE_REGISTRY_ADDR` | Yes | `AnchorStateRegistry` on L1; read to find the scanner's lower bound |
| `CHALLENGER_E2E_ANVIL_PORT` | No (default `18545`) | Fork port; not 8545, which the production challenger reserves for its signer sidecar |
| `CHALLENGER_E2E_CHALLENGER_METRICS_URL` | No (default `http://127.0.0.1:7300/metrics`) | Prometheus endpoint of the challenger under test |
| `CHALLENGER_E2E_GAME_LOOKBACK` | No (default `50`) | Factory indices searched for two games |
| `CHALLENGER_E2E_STARTUP_TIMEOUT` | No (default `5m`) | Budget for the fork and the first scan |
| `CHALLENGER_E2E_QUIET_WINDOW` | No (default `90s`) | Positive-case observation window |
| `CHALLENGER_E2E_DISPUTE_TIMEOUT` | No (default `45m`) | Budget for each SNARK / dispute; sized for a real proof |
| `CHALLENGER_E2E_POLL_INTERVAL` | No (default `5s`) | Driver poll interval |

`anvil` must be on `PATH`. The challenger under test must be reachable on
`CHALLENGER_E2E_CHALLENGER_METRICS_URL` and must be configured with a local
private key rather than a signer sidecar, since the driver supplies the key
through the env file.

## Usage

```toml
[dependencies]
base-challenger-e2e = { workspace = true }
```

```rust,ignore
use base_challenger_e2e::ChallengerE2e;

ChallengerE2e::run().await?;
```

The `base-challenger-e2e` binary wraps this for K8s `CronJob` execution; the pod
topology it expects lives in `protocols/base-proofs`, in
`chart/challenger-e2e`.
