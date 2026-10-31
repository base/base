# base-replay-export

Streams block-range replay exports from local or S3 minute storage, independent of
the builder and Engine API dependencies in `base-replay-driver`.

```sh
base replay export --from 100 --to 120 --rpc-url http://localhost:8545 \
  --network base --store ./recordings --warmup 8s --out ./export
```

Blocks N through M are inclusive. The record window is half-open:
`[max(0, ts(N) * 1000 - warmup_ms), ts(M) * 1000 + 2000)`.
The end uses a fixed two-second block-time allowance, not the next header. It
includes the final block's ingress window even at the RPC tip, without requesting
M+1. For subsecond chains this intentionally overcaptures after M. Header times
are Unix seconds; record and manifest times are Unix milliseconds. Warmup defaults
to 8s and must be an integral number of milliseconds. The network is the storage
namespace, not an RPC chain selector; `chain_id` identifies the RPC chain.

`--out` must not exist. It receives `records.jsonl` containing `ReplayRecord`
objects in `replay-record/v1` format and `manifest.json` with format
`replay-export/v1`, record format, network, chain ID, inclusive block numbers,
endpoint block timestamps, warmup, inclusive `start_ms`, exclusive `end_ms`, and
record count. Consumers must check both format identifiers before loading the
fixed `records.jsonl` filename. The manifest is published last; its absence means
an incomplete export. Failed or cancelled exports may leave a partial directory;
use a fresh output path to retry. Existing outputs are never overwritten.

Records stream in timestamp order across minutes, stable within timestamp ties.
Memory is bounded by one store minute plus one serialized record, not the range.
Missing minutes are skipped, not proof of recording coverage. Empty exports are
valid. Malformed or unreadable minutes fail rather than silently truncate.

For S3 use `--store s3://bucket/prefix`, optional `--s3-endpoint <url>` and
`--s3-region <region>`. Credentials use the AWS SDK chain, including
`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and `AWS_SESSION_TOKEN`. S3 options
are rejected for local stores. No credentials or RPC URL enter the manifest.
