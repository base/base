# Forwarder wire fixture

`insert-validated-transaction.json` is the unmodified HTTP request body captured
from the real `TxForwardingService::spawn_requests` transport, carrying an
`InsertValidatedTransaction` with a signed devnet EIP-1559 transfer from the
repository's Alice account to Bob. It is not a production transaction or a devnet
capture: the opt-in test runs the real forwarder against an ephemeral local HTTP
listener, without the pool reader. This isolates the wire format without starting
the Docker L1/L2 stack.

Regenerate from the repository root:

```sh
REPLAY_FIXTURE_PATH="$PWD/crates/utilities/replay/testdata/insert-validated-transaction.json" \
  cargo test -p base-tx-forwarding --test capture_replay_fixture -- \
  --ignored --nocapture
```

The listener saves the HTTP body bytes without JSON reserialization. This fixture
is an incoming JSON-RPC batch containing one call, not a replay record; schema
tests wrap that call's method and raw params in a record and verify replay
serialization preserves those params.
