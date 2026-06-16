# `base-da-server`

HTTP server for alt-DA storage on an L2 rollup. The batcher uploads compressed L2 batch frames here;
nodes fetch them by commitment while deriving the L2 chain from the parent L1.

Batch bytes live in S3 (or a local directory). Only a small **generic commitment** (34 bytes)
is posted to the parent L1.

Library implementation: `crates/infra/alt-da` (`base-alt-da`).

## Configuration

| Flag / env | Default | Description |
|------------|---------|-------------|
| `--port` / `BASE_DA_PORT` | `2583` | HTTP listen port |
| `--da-url` / `BASE_DA_URL` | (required) | Backing store URL |

`BASE_DA_URL` schemes:

- `file:///path` — local directory (dev/tests). Created if missing.
- `s3://bucket/prefix` — S3 bucket and key prefix. Uses default AWS credential chain.

## HTTP API

| Method | Path | Body | Response |
|--------|------|------|----------|
| `GET` | `/health` | — | `200` (liveness; no store I/O) |
| `POST` | `/put` | Raw batch bytes | `200` + commitment bytes (34-byte generic commitment) |
| `GET` | `/get/0x{hex}` | — | `200` + stored bytes, or `404` if missing |

Commitments use the generic format: `0x01` type byte, `0xff` sentinel, then 32 random bytes.
The batcher posts the returned commitment to the parent L1; derivation resolves it via `GET /get/0x…`.

If `POST /put` fails or the server crashes mid-write, retry the whole request. Each retry gets a new commitment.

Objects are keyed by **base64url(commitment)** under the store prefix, not by the hex string in the URL.

## Local development

Build and run with a file backend:

```bash
cargo build -p base-da-server-bin
BASE_DA_URL=file:///tmp/l3-da BASE_DA_PORT=2583 ./target/debug/base-da-server
```

In another terminal, upload and fetch:

```bash
# Upload batch bytes; save commitment returned by the server
curl -X POST http://127.0.0.1:2583/put --data-binary 'hello-batch' \
  -o /tmp/commitment.bin

COMM=$(xxd -p /tmp/commitment.bin | tr -d '\n')

# Fetch by commitment
curl "http://127.0.0.1:2583/get/0x${COMM}"
# => hello-batch
```

On disk, the blob is stored at `/tmp/l3-da/<base64url-key>` (not `/tmp/commitment.bin`).
List it with `ls /tmp/l3-da/`.

## S3

```bash
BASE_DA_URL=s3://my-bucket/my-prefix \
BASE_DA_PORT=2583 \
./target/debug/base-da-server
```

Requires IAM read/write permissions on the bucket prefix.

## Tests

```bash
cargo test -p base-alt-da
```

Covers commitment encoding, file store roundtrip, and HTTP put/get.
