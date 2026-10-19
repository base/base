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

Dry runs estimate gas and enforce fee caps without signing or submitting. Logs go to stderr; `RUST_LOG` controls their level. A not-ready withdrawal logs its earliest known L1 ready timestamp. An unresolved dispute game can delay finalization further. Run the command again after the proof and game delays have elapsed. Errors print their cause chain and exit non-zero.

Build with `--features ledger` for hardware signing. See the [library README](../../crates/infra/withdrawer/README.md) for signer security, contract checks, fee policy, and timeout behavior.
