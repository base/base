# `base-alt-da`

Alt-DA HTTP server and object stores for L2 batch data.

The batcher uploads L2 batch bytes via `POST /put` and receives a generic commitment to post on
the parent L1. Nodes fetch bytes with `GET /get/0x{hex}` while deriving the L2 chain. Objects are
stored under base64url(commitment) keys in S3 or on disk.

Runnable binary: `bin/da-server` (`base-da-server`).

Batcher dual-write uses [`Client::put`](crate::Client::put) and [`encode_commitment_tx_data`](crate::encode_commitment_tx_data).

## Usage

```rust,ignore
use base_alt_da::{Config, Server, StoreOpener};
use tokio_util::sync::CancellationToken;

let store = StoreOpener::open("file:///tmp/l3-da").await?;
let server = Server::new(Config { port: 2583, da_url: "file:///tmp/l3-da".into() }).await?;
server.run(CancellationToken::new()).await?;
```

## PUT semantics

`POST /put` generates a new random commitment, stores the body, then returns the commitment bytes.
If the request fails or the server crashes before the write completes, the batcher must retry
the whole `POST /put`. Each retry produces a new commitment.

## Limits

`MAX_OBJECT_BYTES` is `8 * base_protocol::BLOB_MAX_DATA_SIZE` (see `lib.rs`).
