# `base-batcher-cli`

Arguments and startup for `base batcher`.

Submits L2 batch data to the L1 DA layer. Wraps `base-batcher-service` with
CLI argument parsing and signal handling.

Logging and metrics use the shared `base` flags and `BASE_NODE_LOG_*` /
`BASE_NODE_METRICS_*` environment variables.

L1 RPC uses `--l1-rpc-url` / `BASE_NODE_L1_ETH_RPC`. Top-level `--chain` /
`BASE_CHAIN` selection is rejected. Chain details and the batch inbox are fetched
from the rollup config of the node whose derivation the batcher follows.

## Sequencers

`--sequencer-urls` / `BASE_BATCHER_SEQUENCER_URLS` takes the sequencer HTTP
endpoints, comma-separated. The batcher submits the unsafe blocks of the leader,
read with `eth_getBlockByNumber`. A single endpoint is the leader. Among several,
the leader is the first whose `admin_sequencerActive` answers `true`, and one that
answers anything else or does not answer is not active. The batcher waits for a
leader at startup, up to `--wait-node-sync-timeout`, then looks for it again every
`--poll-interval`, and keeps reading from the last one while none is.

A canonical batcher also reads the rollup config of the leader
(`optimism_rollupConfig`) and follows its derivation (`optimism_syncStatus`).
Unless `--no-throttle` is set, it pushes the DA limits to every endpoint over
`miner_setMaxDASize`, so the block builder of a sequencer that becomes the
leader already applies them. An endpoint that answers method not found to
`miner_setMaxDASize` stops the batcher.

An endpoint must serve all the methods the batcher calls on it. A conductor with
its RPC proxy enabled does, and so does a consensus node started with
`--rpc.execution-forwarding-endpoint`, which forwards the methods it does not
serve itself, like `eth_getBlockByNumber` and `miner_setMaxDASize`, to its
execution client. Among several endpoints, a consensus node also needs
`--rpc.enable-admin` to serve `admin_sequencerActive`.

## Configuration

`--compressed-size-target` optionally closes a channel after an accepted batch
reaches the target. `--max-blobs-per-tx` caps blob packing per L1 transaction,
while `--max-calldata-size-bytes` caps calldata transactions. `--brotli-quality`
selects Brotli quality `0..=11` (default 9). `--data-availability-type`
selects blobs or calldata; `--max-channel-duration` and `--sub-safety-margin`
control channel lifetime. For calldata configurations,
`--no-force-blobs-when-throttling` disables the throttle-driven blob override.
`--network-timeout` bounds the RPC calls to L1, the sequencers, the parity
validator and the block builders, so an endpoint that stops answering never
blocks the batcher. The corresponding environment variables use the
`BASE_BATCHER_` prefix.

## Shadow mode

`base batcher` follows the derivation of one rollup node. It reads
`batch_inbox_address` from that node's `optimism_rollupConfig` response and
posts DA transactions to that inbox, which the node derives its chain from.

Outside shadow mode, `base batcher` refuses to start unless its signer is the
current `SystemConfig` batcher address, since derivation would ignore every
batch it posts.

A shadow deployment (`--shadow.enabled`) posts to a non-canonical inbox, the
shadow inbox, that an isolated parity validator derives its chain from. It
submits the unsafe blocks of the leader among `--sequencer-urls` and follows
the derivation of the parity validator.
`--shadow.validator-rollup-rpc` points at the validator's rollup node, whose
rollup config names the shadow inbox. `--shadow.inbox` names it as well, and the
batcher refuses to start if the batch inbox of that config is another one. The
parity validator must accept the shadow inbox and signer, since derivation
filters batches by both. `--shadow.validator-l2-rpc` points at the
validator's L2 RPC, whose derived blocks the batcher compares with the leader
sequencer's. The batcher refuses to start with only some of `--shadow.enabled`,
`--shadow.inbox`, `--shadow.validator-rollup-rpc` and
`--shadow.validator-l2-rpc`. The batcher would push its DA limits to the
canonical sequencers, so it refuses to start in shadow mode without
`--no-throttle`. Shadow deployments can use either the local `--private-key`
signer or the production remote-signer path with `--signer-endpoint` and
`--signer-address`.
