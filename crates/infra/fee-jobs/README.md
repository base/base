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

`fee-jobs confirm` finds recent L2 `FeesDisbursed` events and proves or finalizes each receipt's withdrawals on L1 through `base-withdrawer`. It supports fault proofs only. It shares RPC providers and one signer across withdrawals and waits for inclusion before preparing the next transaction.

| Flag | Environment variable | Default |
| --- | --- | --- |
| `--l1-rpc-url` | `L1_RPC_URL` | Required |
| `--l2-rpc-url` | `L2_RPC_URL` | Required |
| `--optimism-portal-proxy-address` | `OPTIMISM_PORTAL_PROXY_ADDRESS` | Required |
| `--dispute-game-factory-address` | `DISPUTE_GAME_FACTORY_ADDRESS` | Required |
| `--anchor-state-registry-address` | `ANCHOR_STATE_REGISTRY_ADDRESS` | Required |
| `--l2-fee-disburser-address` | `L2_FEE_DISBURSER_ADDRESS` | Required |
| `--job-runner-key` | `JOB_RUNNER_KEY` | Required |
| `--num-blocks-to-query` | `NUM_BLOCKS_TO_QUERY` | `1296000` |
| `--num-blocks-per-query` | `NUM_BLOCKS_PER_QUERY` | `3000` |
| `--withdrawal-timeout` | `WITHDRAWAL_TIMEOUT` | `10m` |
| `--dry-run` | None | `false` |

RPC URLs must use HTTP or HTTPS, and addresses must be nonzero. The lookback must be smaller than the current L2 block number. Log-query chunks contain at most the configured positive block count, cover both ends of `[current - lookback, current]`, and pause 300ms between requests. Each block-number, receipt, log-query, and proof-age read is bounded by 30 seconds. Transaction hashes are deduplicated in block/log order.

The job skips already finalized withdrawals, submits proofs for unproven withdrawals, and finalizes ready proofs. A newly proven withdrawal stays pending until a later run. Waiting for proof maturity or dispute-game finality also counts as pending. Portal readiness/finalization reverts for proof age or invalid root claims count as pending; an already-finalized revert is skipped. Pending logs include the onchain proof age against the local clock, or "just proven" for a zero timestamp. A pause, missing covering game, any proving error, or an unrecognized error fails that withdrawal. The job continues through every withdrawal, logs pending and failed hashes with counts, and exits nonzero if any receipt or withdrawal failed. Pending withdrawals alone exit successfully.

`--dry-run` performs decisions, proof verification, gas estimation, and transaction preparation without signing or broadcasting. Logs distinguish prepared proofs/finalizations from real submissions. It still needs a valid signing key to select the submitter's proof. Automatic fees and a gas multiplier of `1.0` are used; the transaction-manager flags do not apply to `confirm`. Keep the key in the environment rather than argv. A per-withdrawal timeout after signing reports the signed L1 transaction hash, which may already have been broadcast or included. Check that hash before retrying. The timeout does not undo a submission or bound the entire run, so choose the lookback and timeout to fit the job's overall deadline.

Run `fee-jobs <subcommand> --help` for configuration, including logging options under `FEE_JOBS_LOG_*`.
