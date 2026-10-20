# Fee jobs

One-shot fee collection jobs for Base. The `fee-jobs` binary runs one job and exits:

| Subcommand | RPC environment variable | Contract environment variable | Precondition |
| --- | --- | --- | --- |
| `disburse` | `L2_RPC_URL` | `L2_FEE_DISBURSER_ADDRESS` | Latest L2 block timestamp is at least the last disbursement time plus the interval |
| `process` | `L1_RPC_URL` | `L1_BALANCE_TRACKER_ADDRESS` | Balance is at least 1 ETH |
| `sweep` | `L2_RPC_URL` | `L2_REGISTRAR_CONTROLLER_ADDRESS` | Balance is at least 0.01 ETH |

These three subcommands also require `JOB_RUNNER_KEY`, a local signing key in hex, with or without a `0x` prefix. They require no other addresses or RPC URLs. A false precondition logs the reason and exits successfully without sending. RPC errors, send errors, timeouts, and reverted receipts fail the job.

All four commands use the transaction manager for gas and fee estimation, fee-bumped resubmission and confirmation depth. Its `--tx-manager.*` flags and `FEE_JOBS_TX_MANAGER_*` environment variables apply to `confirm` too. Defaults include ten confirmations and a ten-minute send timeout. A timeout does not undo a transaction already published to the network.

Each run takes its first nonce from the `pending` block tag, so jobs that share `JOB_RUNNER_KEY` on one chain and overlap queue behind each other instead of replacing each other's transactions. The trade-off: if a run times out and leaves its transaction stuck in the mempool, later runs sign the next nonce and queue behind it, so they also time out until the stuck transaction is mined or evicted. To recover sooner, replace or cancel the stuck nonce by hand.

## Confirm disbursals

`fee-jobs confirm` finds recent L2 `FeesDisbursed` events and proves or finalizes the `FeeDisburser`'s own ETH bridge withdrawals on L1 through `base-withdrawer`. Other withdrawals in each receipt are skipped with a warning; a receipt with no matching withdrawals fails. It supports fault proofs only. It shares RPC providers and one signer across withdrawals and waits for inclusion before preparing the next transaction.

| Flag | Environment variable | Default |
| --- | --- | --- |
| `--l1-rpc-url` | `L1_RPC_URL` | Required |
| `--l2-rpc-url` | `L2_RPC_URL` | Required |
| `--l1-optimism-portal-address` | `L1_OPTIMISM_PORTAL_ADDRESS` | Required |
| `--l2-fee-disburser-address` | `L2_FEE_DISBURSER_ADDRESS` | Required |
| `--job-runner-key` | `JOB_RUNNER_KEY` | Required |
| `--lookback` | `FEE_JOBS_CONFIRM_LOOKBACK` | `21d` |
| `--l2-block-time` | `FEE_JOBS_CONFIRM_L2_BLOCK_TIME` | `2s` |
| `--num-blocks-per-query` | `FEE_JOBS_CONFIRM_NUM_BLOCKS_PER_QUERY` | `3000` |
| `--withdrawal-timeout` | `FEE_JOBS_CONFIRM_WITHDRAWAL_TIMEOUT` | `10m` |
| `--dry-run` | None | `false` |

Before scanning, the job reads `disputeGameFactory()` and `anchorStateRegistry()` from the configured portal once and uses those contracts for every withdrawal. Discovery failures and zero portal addresses fail the run, including dry runs.

RPC URLs must use HTTP or HTTPS, and addresses must be nonzero. The lookback must cover the slowest path to finalization under the portal's respected game type, the `AggregateVerifier` (game type 621) on Base mainnet. That path is a successful challenge. A game with one proof resolves after `SLOW_FINALIZATION_DELAY` (5 days), and a ZK challenge resets that delay from the challenge time. The challenged game and its descendants then resolve as `CHALLENGER_WINS`, the withdrawal's proof is no longer eligible, and the job re-proves it against a new game on a later run. That game needs its own `SLOW_FINALIZATION_DELAY`. With the proof on day 0 or 1, a challenge by about day 5, resolution about day 10, and a re-prove on the next run, finalization lands around day 16 or 17. The 21-day default leaves about 4 days for cron intervals and slack. The portal's `proofMaturityDelaySeconds` (1 day) runs concurrently with game resolution, and the anchor state registry's `disputeGameFinalityDelaySeconds` is 0. The lookback and configured L2 block time must be positive durations. The job divides the lookback by the block time, rounds up to a block count, and scans `[current.saturating_sub(blocks), current]`, including block 0 when the lookback reaches past genesis.

Log-query chunks contain at most the configured positive block count and cover both ends of the window without gaps or overlaps. Keep `--num-blocks-per-query` configurable for RPC limits; for example, sepolia.base.org allows about 200 blocks per query. With the defaults and sufficient chain history, the scan covers 907,201 blocks in 303 log queries. There is no fixed pause between chunks. For block-number, receipt, log-query, and proof-age reads, the `base-withdrawer` transport bounds each HTTP attempt to the smaller of 30 seconds and half the withdrawal timeout, with at most three retries and exponential backoff, including for HTTP 429 rate-limit responses. Transaction hashes are deduplicated in block/log order.

The job skips already finalized withdrawals, submits proofs for unproven withdrawals, and finalizes ready proofs. A newly proven withdrawal stays pending until a later run. Waiting for proof maturity or dispute-game finality also counts as pending. A finalization that may already be onchain fails without a status recheck if its target call failed, its finalization event is missing, or it is unconfirmed after signing. For other finalization errors, the job rechecks the withdrawal's status once: finalized withdrawals are skipped, and withdrawals that are no longer ready or need a new proof count as pending. Any other status or a failed recheck returns the original finalization error, with recheck failures attached as context.

Pending logs include the onchain proof age against the local clock, or "just proven" for a zero timestamp. A failed proof-age read is logged and does not fail the withdrawal. A pause, missing covering game, any proving error, or a status-read error fails that withdrawal. The job continues through every matching withdrawal, logs pending and failed hashes with counts, and exits nonzero if any receipt or withdrawal failed. Pending withdrawals alone exit successfully.

`--dry-run` performs decisions, proof verification and gas/cost estimation without signing, broadcasting or constructing a transaction manager. Logs distinguish prepared proofs/finalizations from real submissions. It still needs a valid key to select the submitter's proof. Keep the key in the environment rather than argv. The shared transaction-manager fee minimums apply to the estimate.

For real sends, one wallet-backed `SimpleTxManager` uses `TxManager::send`, refreshes its pending nonce before each submission and closes at the end. Configuration rejects a submission network timeout that leaves no time for HTTP attempts after retry delays. The withdrawer's submission transport journals broadcast hashes before forwarding requests. Every post-publish failure, including the per-withdrawal deadline expiring during a send, reports all broadcast L1 hashes, including replacements. Before rerunning, check these hashes for inclusion or wait for them to mine or drop. Withdrawal status reads use only the latest mined L1 state, so a rerun while a transaction is pending can submit a duplicate at the next nonce that reverts and spends gas after the first mines. The deadline does not undo a submission or bound the entire run, so choose the lookback duration, chunk size and withdrawal timeout to fit the overall job deadline.

Run `fee-jobs <subcommand> --help` for configuration, including logging options under `FEE_JOBS_LOG_*`.
