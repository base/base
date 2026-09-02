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
delta on B. Both are generated per run and never leave the pod.

## What it asserts

One process, one fork, two TEE-only in-progress games (newest-first, lookback
50, ≥1 intermediate root, all above the anchor game). Game A is Path 1 / Path 2
skip. Game B is Path 4 and then whichever path Path 4 leaves behind. The run
bails if fewer than two such games exist.

The anchor bound is not cosmetic: the scanner starts at one past the anchor
game's factory index, so a game at or before the anchor is one the challenger
will never look at, however invalid it is made — the dispute waits below would
sit out their whole timeout on a game nobody was watching.

Every game in the lookback window other than A and B is snapshotted before the
challenger boots and re-read at the end (step 7). The per-path assertions below
are all scoped to A or B, so without that bound a challenger that *also*
disputes games it was never given would pass the run.

1. **The challenger comes up.** `base_challenger_up` is 1 and at least one scan
   has completed.
2. **Positive case.** `games_invalid_total`, `nullify_tx_submitted_total` and
   `challenge_tx_submitted_total` must be zero outright on the first post-scan
   scrape and still zero after `CHALLENGER_E2E_QUIET_WINDOW` — including
   against the still-valid dual-proof game B. A challenger that disputes valid
   games fails here.

   The baseline is absolute rather than a delta because
   `games_scanned_total` is incremented for the whole scanned range *before*
   any candidate is validated, so a challenger that disputed during startup
   would otherwise be absorbed into the baseline and pass.

   Progress over the window is asserted on
   `validation_latency_seconds_count` minus `validation_errors_total`, not on
   `games_scanned_total`. The last counts attempted factory indices and
   advances even when every game query fails. The histogram is closer — it is
   only touched from inside the validator — but its latency is recorded from a
   drop guard, so it too counts attempts rather than successes. Subtracting the
   error counter, which the validator increments exactly once per failed call,
   leaves the games the challenger actually managed to check.

   Validation errors below that threshold are reported rather than fatal —
   they are usually the L2 RPC rather than the challenger.
3. **Path 1 `InvalidTeeProposal`.** After game A's last intermediate root is
   corrupted, B must either clear `teeProver()` (TEE nullify) or set
   `zkProver()` and `counteredByIntermediateRootIndexPlusOne()` (ZK challenge).
   Both are correct — the challenger tries TEE first and falls back to ZK.
   B's nonce must move.

   A challenge is checked for *what* it challenged, not just that it happened:
   `counteredByIntermediateRootIndexPlusOne` must name the root this run
   corrupted, and `zkProver()` must be B. An accepted proof against some other
   checkpoint clears the "was it disputed" bar without disputing the
   corruption, and would otherwise pass. Both are asserted after the poll
   rather than inside it — the poll retries on error, so an assertion in there
   would surface as a timeout instead of as the mismatch it is.
4. **Game A settles.** One quiet window on A, whichever way Path 1 landed. B's
   nonce must not move either: a dispute that reverts changes none of the three
   fields, so a challenger stuck re-challenging a legitimate challenge or
   re-nullifying an already-nullified game is invisible to the state comparison
   on its own. Nothing else on the fork is disputable for the length of the
   window — game B is still valid, the bystanders always were — so any new
   transaction at all is the finding. A fee-bumped replacement reuses its
   nonce, so retries do not trip this. If
   it was a ZK challenge this is **Path 2 skip**: `zkProver` and
   `counteredIndex` stay set, and a challenger that "defends" a legitimate
   challenge of a wrong TEE root fails here. If it was a TEE nullify there is
   no challenge to leave standing, and the same window proves **idempotence**:
   the challenger must not dispute a game it has already nullified. Path 2
   *dispute* (fraudulent ZK against a correct TEE root) is not staged: the real
   prover cannot produce a wrong-root proof the real verifier accepts. That
   half stays in the mock driver tests.
5. **Path 4 `InvalidDualProposal`.** Game B was staged *before* the challenger
   was released: A requested a real SNARK of B's canonical roots from
   `BASE_CHALLENGER_ZK_RPC_URL` (not the fork) and submitted
   `verifyProposalProof`. `zkProver != 0` and `counteredIndex == 0`. After the
   quiet window, B is patched. The challenger must drop one of B's two proofs.
   B's nonce must advance.
6. **Whatever Path 4 left behind.** A dual-proof game takes two disputes to
   clear, and either proof may go first — so which assertion runs is decided by
   what step 5 observed, not fixed in advance. TEE first (`tee=0`, `zk≠0`) is
   **Path 3**: the next scan ZK-nullifies (`zkProver == 0`). ZK first is the
   supported **TEE-fallback** case, where the TEE request or submission failed;
   that leaves a TEE-only game (`tee≠0`, `zk=0`) and the next scan disputes it
   as **Path 1**, by nullify or by challenge. Insisting on the TEE proof going
   first would sit out the whole `CHALLENGER_E2E_DISPUTE_TIMEOUT` on a
   correctly behaving challenger.

   Step 5 reads both prover fields in one observation, and a challenger that
   scans faster than `CHALLENGER_E2E_POLL_INTERVAL` may have cleared both
   before the first look; that is a third branch, not a failure. Attribution is
   therefore one assertion at the end — B's nonce must have advanced by at
   least two against the baseline taken before the patch — rather than one per
   step. A per-step delta credits both transactions to the first step whenever
   the challenger beats the poll, and then demands a third that is never
   coming.
7. **No collateral damage.** Every bystander game snapshotted in step 0 must
   still read the same `(teeProver, zkProver, counteredIndex)`. Catches what
   the per-game assertions cannot see: a challenger misconfigured on
   `game_type`, one with a broken lookback, or one that starts disputing
   indiscriminately after its first dispute.

   Games whose prover fields do not read **when snapshotted** are left out of
   the watch set rather than failing the run: they are a different verifier
   shape, so the challenger cannot move them through the fields this test
   watches. The re-read at the end is not lenient in the same way. Every game
   in the set already read cleanly once, so a read that fails now is the RPC,
   not a shape mismatch — and skipping it would quietly drop a game from the
   only assertion that catches indiscriminate disputing. It fails the run, with
   a message that says the check could not be completed rather than that the
   challenger moved something.

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
| `BASE_CHALLENGER_ANCHOR_STATE_REGISTRY_ADDR` | Yes | `AnchorStateRegistry` on L1; read to find the scanner's lower bound |
| `CHALLENGER_E2E_ANVIL_PORT` | No (default `18545`) | Fork port; not 8545, which the production challenger reserves for its signer sidecar |
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
