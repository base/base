# System Tests (Integration Suite)

Integration coverage for TIPS ingress RPC. Tests talk to the services started by `just start-all`.

## What we test
- `test_client_can_connect_to_tips` – RPC connectivity.
- `test_send_raw_transaction_accepted` – `eth_sendRawTransaction` lands on-chain with success receipt.
- `test_send_bundle_accepted` – single‑tx bundle via `eth_sendBackrunBundle` returns the correct bundle hash, audit event, and on-chain inclusion.
- `test_send_bundle_with_two_transactions` – multi-tx bundle (2 txs) flows through audit and lands on-chain.

Each test confirms:
1. The response hash equals `keccak256` of the tx hashes.
2. The bundle audit event is emitted to Kafka.
3. All transactions are included on-chain with successful receipts.

## How to run
```bash
# Start infrastructure (see ../../SETUP.md for full instructions)
#  - just sync && just start-all
#  - builder-playground + op-rbuilder are running

# Run the tests
INTEGRATION_TESTS=1 cargo test --package tips-system-tests --test integration_tests
```

**Note:** Tests that share the funded wallet use `#[serial]` to avoid nonce conflicts.

Defaults:
- Kafka configs: `docker/host-*.properties` (override with the standard `TIPS_INGRESS_KAFKA_*` env vars if needed).
- URLs: `http://localhost:8080` ingress, `http://localhost:8547` sequencer (override via `INGRESS_URL` / `SEQUENCER_URL`).
