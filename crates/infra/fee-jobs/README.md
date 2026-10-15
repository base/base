# Fee jobs

One-shot fee collection jobs for Base. The `fee-jobs` binary runs one job and exits:

| Subcommand | RPC environment variable | Contract environment variable | Precondition |
| --- | --- | --- | --- |
| `disburse` | `L2_RPC_URL` | `L2_FEE_DISBURSER_ADDRESS` | Latest L2 block timestamp is at least the last disbursement time plus the interval |
| `process` | `L1_RPC_URL` | `L1_BALANCE_TRACKER_ADDRESS` | Balance is at least 1 ETH |
| `sweep` | `L2_RPC_URL` | `L2_REGISTRAR_CONTROLLER_ADDRESS` | Balance is at least 0.01 ETH |

Each subcommand also requires `JOB_RUNNER_KEY`, a local signing key in hex, with or without a `0x` prefix. No other addresses or RPC URLs are required. A false precondition logs the reason and exits successfully without sending. RPC errors, send errors, timeouts, and reverted receipts fail the job.

The transaction manager estimates gas and fees and resubmits with bumped fees. Its optional flags and `FEE_JOBS_TX_MANAGER_*` environment variables configure confirmations and timeouts. The default is ten confirmations and a ten-minute overall send timeout. A timeout does not cancel a transaction already published to the network.

Each run takes its first nonce from the `pending` block tag, so jobs that share `JOB_RUNNER_KEY` on one chain and overlap queue behind each other instead of replacing each other's transactions. The trade-off: if a run times out and leaves its transaction stuck in the mempool, later runs sign the next nonce and queue behind it, so they also time out until the stuck transaction is mined or evicted. To recover sooner, replace or cancel the stuck nonce by hand.

Run `fee-jobs <subcommand> --help` for configuration, including logging options under `FEE_JOBS_LOG_*`.
