# Local Devnet Transaction Inclusion

Start the local devnet, send an ETH transfer directly to the sequencer, and confirm it was included successfully.

## Before starting

Use Docker, `just`, and Foundry's `cast`. Startup and shutdown delete `.devnet` data; use a disposable devnet and check for conflicting containers/ports first. Never stop unrelated services. The devnet publishes ports on all interfaces, so use an isolated development machine.

Use only the public test accounts in `etc/docker/devnet-env` against the local devnet. That file and `etc/docker/Justfile` are the source of truth for accounts, ports, and commands.

## Verify

From the repository root:

```bash
just devnet up-single
source etc/docker/devnet-env
cast send "$ANVIL_ACCOUNT_2_ADDR" --value 0.001ether \
  --private-key "$ANVIL_ACCOUNT_1_KEY" \
  --rpc-url http://127.0.0.1:7545 --chain-id 84538453 --json
```

`cast send` waits for the receipt. Require status `0x1`, the expected sender/recipient, and a block number/hash. Use `cast block <block-number> --rpc-url http://127.0.0.1:7545 --json` to confirm the block hash matches the receipt and its transactions include the submitted hash. A transaction hash alone is not a pass; this verifies sequencer inclusion, not L1 finality or validator synchronization.

If startup or submission fails, inspect `just devnet ps` and `just devnet logs base-builder` before retrying. Do not blindly resend after a timeout: the transaction may already have been broadcast.

Report the transaction hash, receipt status, and block number/hash. Shut down only the devnet you started with `just devnet down`, unless asked to leave it running.
