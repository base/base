# Fee jobs

One-shot fee collection jobs for Base. The `fee-jobs` binary runs one job and exits:

| Subcommand | RPC environment variable | Contract environment variable | Precondition |
| --- | --- | --- | --- |
| `disburse` | `L2_RPC_URL` | `L2_FEE_DISBURSER_ADDRESS` | Latest L2 block timestamp is at least the last disbursement time plus the interval |
| `process` | `L1_RPC_URL` | `L1_BALANCE_TRACKER_ADDRESS` | Balance is at least 1 ETH |
| `sweep` | `L2_RPC_URL` | `L2_REGISTRAR_CONTROLLER_ADDRESS` | Balance is at least 0.01 ETH |

These three subcommands also require `JOB_RUNNER_KEY`, a local signing key in hex, with or without a `0x` prefix. They require no other addresses or RPC URLs. A false precondition logs the reason and exits successfully without sending. RPC errors, send errors, timeouts, and reverted receipts fail the job.

For these three commands, the transaction manager estimates gas and fees and resubmits with bumped fees. Its optional flags and `FEE_JOBS_TX_MANAGER_*` environment variables configure confirmations and timeouts. The default is ten confirmations and a ten-minute overall send timeout. A timeout does not cancel a transaction already published to the network.

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
| `--num-blocks-to-query` | `FEE_JOBS_CONFIRM_NUM_BLOCKS_TO_QUERY` | `1296000` |
| `--num-blocks-per-query` | `FEE_JOBS_CONFIRM_NUM_BLOCKS_PER_QUERY` | `3000` |
| `--withdrawal-timeout` | `FEE_JOBS_CONFIRM_WITHDRAWAL_TIMEOUT` | `10m` |
| `--dry-run` | None | `false` |

Before scanning, the job reads `disputeGameFactory()` and `anchorStateRegistry()` from the configured portal once and uses those contracts for every withdrawal. Discovery failures and zero portal addresses fail the run, including dry runs.

RPC URLs must use HTTP or HTTPS, and addresses must be nonzero. The lookback must be smaller than the current L2 block number. Log-query chunks contain at most the configured positive block count, cover both ends of `[current - lookback, current]`, and pause 300ms between requests. For block-number, receipt, log-query, and proof-age reads, the transport bounds each HTTP attempt to the smaller of 30 seconds and half the withdrawal timeout, with at most three retries and exponential backoff. Transaction hashes are deduplicated in block/log order.

The job skips already finalized withdrawals, submits proofs for unproven withdrawals, and finalizes ready proofs. A newly proven withdrawal stays pending until a later run. Waiting for proof maturity or dispute-game finality also counts as pending. A finalization that may already be onchain fails without a status recheck if its target call failed, its finalization event is missing, or it is unconfirmed after signing. For other finalization errors, the job rechecks the withdrawal's status once: finalized withdrawals are skipped, and withdrawals that are no longer ready or need a new proof count as pending. Any other status or a failed recheck returns the original finalization error, with recheck failures attached as context.

Pending logs include the onchain proof age against the local clock, or "just proven" for a zero timestamp. A failed proof-age read is logged and does not fail the withdrawal. A pause, missing covering game, any proving error, or a status-read error fails that withdrawal. The job continues through every matching withdrawal, logs pending and failed hashes with counts, and exits nonzero if any receipt or withdrawal failed. Pending withdrawals alone exit successfully.

`--dry-run` performs decisions, proof verification, gas estimation, and transaction preparation without signing or broadcasting. Logs distinguish prepared proofs/finalizations from real submissions. It still needs a valid signing key to select the submitter's proof. Automatic fees and a gas multiplier of `1.0` are used; the transaction-manager flags do not apply to `confirm`. Keep the key in the environment rather than argv. A per-withdrawal timeout after signing reports the signed L1 transaction hash, which may already have been broadcast or included. Check that hash before retrying. The timeout does not undo a submission or bound the entire run, so choose the lookback and timeout to fit the job's overall deadline.

Run `fee-jobs <subcommand> --help` for configuration, including logging options under `FEE_JOBS_LOG_*`.
