# `base-challenger-e2e`

Behavioural end-to-end test of the challenger.

Forks the target L1 into a pod-local Anvil, hands the fork to a real
`base-challenger` binary running alongside it, and asserts on what that
challenger does — first that it leaves valid games alone, then that it
disputes every classifier path we can stage honestly on that same fork.

Patching an existing game rather than creating one is what keeps the test
honest. The games were created and verified on the real chain before the fork
point, so the `TEEVerifier`, the `TEEProverRegistry` and the game bytecode are
all real; the challenger's dispute proof is genuinely verified onchain rather
than waved through by a stub. The corruption itself reuses
[`base_zk_fork_dispute::Checkpoint::patch`], which rewrites the CWIA root in the
game bytecode and repairs the factory's `_disputeGames` registration so lookups
still resolve.

Key **A** (driver) signs setup only (`verifyProposalProof` to stage Path 4).
Key **B** (challenger) is the only one that may dispute. Attribution is a nonce
delta on B.

## What it asserts

One process, one fork, two TEE-only in-progress games (newest-first, lookback
50, ≥1 intermediate root). Game A is Path 1 / Path 2 skip. Game B is Path 4→3.
The run bails if fewer than two such games exist.

1. **The challenger comes up.** `base_challenger_up` is 1 and at least one scan
   has completed.
2. **Positive case.** Over `CHALLENGER_E2E_QUIET_WINDOW`,
   `base_challenger_games_scanned_total` advances while
   `games_invalid_total`, `nullify_tx_submitted_total` and
   `challenge_tx_submitted_total` stay flat — including against the still-valid
   dual-proof game B. A challenger that disputes valid games fails here.
3. **Path 1 `InvalidTeeProposal`.** After game A's last intermediate root is
   corrupted, B must either clear `teeProver()` (TEE nullify) or set
   `zkProver()` and `counteredByIntermediateRootIndexPlusOne()` (ZK challenge).
   Both are correct — the challenger tries TEE first and falls back to ZK.
   B's nonce must move.
4. **Path 2 skip** (only if Path 1 was a ZK challenge). One quiet window:
   `zkProver` and `counteredIndex` on A stay set. A challenger that "defends" a
   legitimate challenge of a wrong TEE root fails here. If Path 1 was a TEE
   nullify, this step is skipped (logged, not a failure). Path 2 *dispute*
   (fraudulent ZK against a correct TEE root) is not staged: the real prover
   cannot produce a wrong-root proof the real verifier accepts. That half stays
   in the mock driver tests.
5. **Path 4 `InvalidDualProposal`.** Game B was staged *before* the challenger
   was released: A requested a real SNARK of B's canonical roots from
   `BASE_CHALLENGER_ZK_RPC_URL` (not the fork) and submitted
   `verifyProposalProof`. `zkProver != 0` and `counteredIndex == 0`. After the
   quiet window, B is patched. The challenger must TEE-nullify first
   (`teeProver == 0`). B's nonce must advance.
6. **Path 3 `InvalidZkProposal`.** Same game, next scan after TEE nullify
   (`tee=0`, `zk≠0`, `countered=0`). B ZK-nullifies (`zkProver == 0`). B's
   nonce must advance again.

## Required environment

`BASE_CHALLENGER_*` is shared with the challenger under test — both read the
same config-service mapping, so the driver forks exactly the L1 the challenger
is pointed at and talks to the same prover-service.

| Variable | Required | Purpose |
|----------|----------|---------|
| `BASE_CHALLENGER_L1_ETH_RPC` | Yes | L1 the fork is taken from; only ever read |
| `BASE_CHALLENGER_L2_ETH_RPC` | Yes | L2 archive RPC for canonical output roots |
| `BASE_CHALLENGER_ZK_RPC_URL` | Yes | Live prover-service JSON-RPC for Path 4 setup (not the fork) |
| `BASE_CHALLENGER_DISPUTE_GAME_FACTORY_ADDR` | Yes | `DisputeGameFactory` on L1 |
| `BASE_CHALLENGER_GAME_TYPE` | Yes | `AggregateVerifier` game type |
| `CHALLENGER_E2E_ANVIL_PORT` | No (default `18545`) | Fork port; not 8545, which the production challenger reserves for its signer sidecar |
| `CHALLENGER_E2E_CHALLENGER_ENV_FILE` | No (default `/shared/challenger.env`) | Written to release the challenger sidecar |
| `CHALLENGER_E2E_CHALLENGER_METRICS_URL` | No (default `http://127.0.0.1:7300/metrics`) | Prometheus endpoint of the challenger under test |
| `CHALLENGER_E2E_GAME_LOOKBACK` | No (default `50`) | Factory indices searched for two games to corrupt |
| `CHALLENGER_E2E_STARTUP_TIMEOUT` | No (default `5m`) | Budget for the fork and the first scan |
| `CHALLENGER_E2E_QUIET_WINDOW` | No (default `90s`) | Positive-case (and Path 2 skip) observation window |
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
