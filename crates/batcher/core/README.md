# `base-batcher-core`

Async orchestration core for the Base batcher.

`BatchDriver` is the central type exported by this crate. It is generic over a `Runtime`, a
`BatchPipeline` (frame encoding), an `UnsafeBlockSource` (L2 block delivery), an `L1HeadSource`
(L1 chain head tracking), a `TxManager` (L1 submission), and a `ThrottleClient` (DA limit
application). The driver runs a single `tokio::select!` task that reacts to four concurrent
event streams: new L2 unsafe blocks, settled L1 heads, completed in-flight transaction receipts,
and a periodic DA throttle tick. Each arm of the loop advances the pipeline or adjusts submission
pressure without blocking the others.

`BatchDriverConfig` carries the three parameters the driver needs at construction: the batcher
inbox address on L1, the maximum number of concurrently in-flight transactions, and the drain
timeout used during shutdown to wait for outstanding receipts before abandoning them.

`SubmissionQueue` owns the entire L1 submission lifecycle. It holds the `TxManager`, a
`FuturesUnordered` set of in-flight receipt futures, a counting `Semaphore` for backpressure, and
a boolean txpool-blocked flag. When the driver calls `submit_pending`, the queue loops: it tries
to acquire a semaphore permit, asks the pipeline for the next ready submission, encodes frames as
blobs or calldata depending on the `DaType`, and hands the resulting `TxCandidate` to the
`TxManager`. Each submission spawns a permit-holding future that resolves to a `(SubmissionId,
TxOutcome)` pair when the transaction settles. Confirmed receipts call `pipeline.confirm` and
`pipeline.advance_l1_head`. Failed submissions are requeued. A `TxpoolBlocked` outcome sets a
sticky flag that prevents further submissions until `recover_txpool` successfully cancels the
stuck transaction. On reorg, `SubmissionQueue::discard` drops all in-flight futures and releases
their permits so the freshly reset pipeline is not corrupted by stale completions.

`TxOutcome` represents the three terminal states of an L1 submission: `Confirmed { l1_block }`,
`Failed`, and `TxpoolBlocked`. Failed frames are always requeued for retry; txpool-blocked frames
are also requeued but submission is suspended until the nonce slot is freed.

The throttle subsystem controls how much DA data the sequencer may include per block and per
transaction based on the L1 DA backlog. `ThrottleController` takes a `ThrottleConfig` and a
`ThrottleStrategy` and produces `ThrottleParams` from a raw backlog byte count.
`ThrottleStrategy::Off` disables throttling entirely. `ThrottleStrategy::Step` applies full
intensity when the backlog exceeds the configured threshold. `ThrottleStrategy::Linear` grows
intensity linearly from zero at the threshold to `max_intensity` at twice the threshold, which
matches the op-batcher reference implementation. `ThrottleParams` carries a fractional `intensity`
value and the corresponding `max_block_size` and `max_tx_size` byte limits computed by
interpolating between the upper and lower limits in `ThrottleConfig`. `DaThrottle` wraps a
`ThrottleController` and a `ThrottleClient` with a last-applied dedup cache so that the
`miner_setMaxDASize` RPC call is only issued when the computed limits actually change between ticks.

`ThrottleClient` is the async trait that connects the throttle controller to the block builder.
Its single method, `set_max_da_size`, forwards the per-transaction and per-block byte limits to
the execution client. The canonical implementation calls the `miner_setMaxDASize` RPC method;
`NoopThrottleClient` silently discards all calls and is used when throttling is disabled, allowing
the driver to invoke the same code path in both cases without special casing.

This crate does not perform frame or blob encoding — those are handled by `base-batcher-encoder`
and `base-blobs`. It does not implement L2 block sourcing or L1 head tracking — those come from
`base-batcher-source`. Transaction signing, gas estimation, and confirmation polling belong to
`base-tx-manager`. Service configuration and process startup live in `base-batcher-service`.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
