# `base-challenger-e2e`

Behavioural end-to-end test of the challenger.

Forks the target L1 into a pod-local Anvil, hands the fork to a real
`base-challenger` binary running alongside it, and asserts on what that
challenger does — first that it leaves valid games alone, then that it disputes
a game whose intermediate output root has been corrupted.

Patching an existing game rather than creating one is what keeps the test
honest. The game was created and verified on the real chain before the fork
point, so the `TEEVerifier`, the `TEEProverRegistry` and the game bytecode are
all real; the challenger's dispute proof is genuinely verified onchain rather
than waved through by a stub. The corruption itself reuses
[`base_zk_fork_dispute::Checkpoint::patch`], which rewrites the CWIA root in the
game bytecode and repairs the factory's `_disputeGames` registration so lookups
still resolve.

## What it asserts

1. **The challenger comes up.** `base_challenger_up` is 1 and at least one scan
   has completed.
2. **Positive case.** Over `CHALLENGER_E2E_QUIET_WINDOW`,
   `base_challenger_games_scanned_total` advances while
   `games_invalid_total`, `nullify_tx_submitted_total` and
   `challenge_tx_submitted_total` stay flat. A challenger that disputes valid
   games fails here.
3. **Negative case.** After the newest in-progress TEE-only game has one
   intermediate root corrupted, the challenger must either clear `teeProver()`
   (nullified with a TEE proof) or set `zkProver()` and
   `counteredByIntermediateRootIndexPlusOne()` (challenged with a ZK proof).
   Both are correct responses on this path — the challenger tries TEE first and
   falls back to ZK — so requiring one specific outcome would fail the run
   whenever the TEE prover is briefly unavailable.
4. **The challenger is the one that acted.** The dispute is attributed by the
   challenger's nonce advancing. The driver signs nothing, and it funds the
   challenger from a separate throwaway account, so "the challenger disputed it"
   cannot be confused with "the driver did".

## Required environment

`BASE_CHALLENGER_*` is shared with the challenger under test — both read the
same config-service mapping, so the driver forks exactly the L1 the challenger
is pointed at.

| Variable | Required | Purpose |
|----------|----------|---------|
| `BASE_CHALLENGER_L1_ETH_RPC` | Yes | L1 the fork is taken from; only ever read |
| `BASE_CHALLENGER_L2_ETH_RPC` | Yes | L2 archive RPC for canonical output roots |
| `BASE_CHALLENGER_DISPUTE_GAME_FACTORY_ADDR` | Yes | `DisputeGameFactory` on L1 |
| `BASE_CHALLENGER_GAME_TYPE` | Yes | `AggregateVerifier` game type |
| `CHALLENGER_E2E_ANVIL_PORT` | No (default `18545`) | Fork port; not 8545, which the production challenger reserves for its signer sidecar |
| `CHALLENGER_E2E_CHALLENGER_ENV_FILE` | No (default `/shared/challenger.env`) | Written to release the challenger sidecar |
| `CHALLENGER_E2E_CHALLENGER_METRICS_URL` | No (default `http://127.0.0.1:7300/metrics`) | Prometheus endpoint of the challenger under test |
| `CHALLENGER_E2E_GAME_LOOKBACK` | No (default `50`) | Factory indices searched for a game to corrupt |
| `CHALLENGER_E2E_STARTUP_TIMEOUT` | No (default `5m`) | Budget for the fork and the first scan |
| `CHALLENGER_E2E_QUIET_WINDOW` | No (default `90s`) | Positive-case observation window |
| `CHALLENGER_E2E_DISPUTE_TIMEOUT` | No (default `45m`) | Budget for the dispute; sized for a real SNARK proof |
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
