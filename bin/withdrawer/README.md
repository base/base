# Withdrawer

`withdrawer` proves or finalizes every withdrawal in an L2 transaction receipt. It supports fault-proof portals and legacy output oracles. Each invocation performs at most one step per withdrawal and waits for each submitted L1 transaction before preparing the next.

Run `cargo run -p base-withdrawer-bin -- --help` for configuration and `--version` for the package version. For example, with a signing key in `WITHDRAWER_PRIVATE_KEY`:

```sh
cargo run -p base-withdrawer-bin -- \
  --network base-sepolia \
  --l1-rpc-url "$L1_RPC_URL" \
  --withdrawal "$L2_TRANSACTION_HASH" \
  --dry-run
```

Prove and finalize transactions use `base-tx-manager` for EIP-1559 fee estimation, fee-bumped resubmission and confirmation depth. Configure it with `--tx-manager.*` flags or `WITHDRAWER_TX_MANAGER_*` environment variables. The default is ten confirmations and a ten-minute send timeout; `--timeout` separately bounds the entire withdrawal operation. `--gas-limit` sets a minimum. Gas is always estimated, and this flag cannot bypass a failing estimate. Explicit fee and gas multiplier flags have been removed.

Dry runs log the candidate's target, calldata length and selector, gas limit and estimated gas cost without signing, submitting or constructing a manager. Logs go to stderr; `RUST_LOG` controls their level. A not-ready withdrawal logs its earliest known L1 ready timestamp. Run again after proof and game delays have elapsed. Errors exit nonzero. The submission transport journals every broadcast L1 transaction hash, including fee replacements, before forwarding the request. Failures and cancellation report these hashes so operators can check inclusion. Rerunning is safe because every invocation re-reads withdrawal status before submitting.

Build with `--features ledger` for hardware signing. See the [library README](../../crates/infra/withdrawer/README.md) for signer security, contract checks, fee policy, and timeout behavior.
Each fee-bumped replacement requires another approval on the Ledger; `--tx-manager.resubmission-timeout` controls the fee-bump interval. Both `--timeout` and `--tx-manager.tx-send-timeout` keep running while you approve replacements.
