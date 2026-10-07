# `base-batcher-core`

Async orchestration core for the Base batcher.

`BatchDriver` is the central type exported by this crate. It is generic over a `Runtime`, a
`BatchPipeline` (frame encoding), an `UnsafeBlockSource` (L2 block delivery), an `L1HeadSource`
(L1 chain head tracking) and a `TxManager` (L1 submission). Construction takes
`BatchDriverInputs`: the sources the driver listens to and the L1 head and safe L2 head it
starts from. The initial L1 head seeds the pipeline, so
channel duration is measured from the live L1 tip rather than from block 0. The driver runs a
single `tokio::select!` task that reacts to unsafe L2 blocks, derivation-status updates, L1
heads, completed transaction receipts, admin commands, and cancellation.
Each arm advances the pipeline or adjusts submission pressure without blocking the others.

`BatchDriverConfig` carries the L1 inbox address, in-flight transaction limit, shutdown drain
timeout, DA-throttle submission policy, and whether block ingestion starts stopped.

`SubmissionQueue` owns the entire L1 submission lifecycle. It holds the `TxManager` and a
`FuturesUnordered` set of in-flight receipt futures. When the driver calls `submit_pending`,
the queue sends one L1 transaction per ready submission, as blobs or calldata depending on its
`DaType`, until `max_pending_transactions` are in flight. Each transaction becomes a receipt
future that resolves to a `(SubmissionId, TxOutcome)` pair when it settles. Confirmed receipts
call `pipeline.confirm` and `pipeline.advance_l1_head`. Failed submissions are requeued. A blob
submission that cannot be built into a transaction is fatal: the encoder packs blobs within
protocol limits, so a retry would fail the same way. In-flight transactions survive a pipeline
reset and keep counting against the limit until they settle; the reset pipeline ignores the
stale ids they report.

`TxOutcome` represents the two terminal states of an L1 submission: `Confirmed { l1_block }`
and `Failed`.

The throttle subsystem controls how much DA data the block builders may include per block and per
transaction based on the L1 DA backlog. `ThrottleController` takes a `ThrottleConfig` and a
`ThrottleStrategy` and produces `ThrottleParams` from a raw backlog byte count.
`ThrottleStrategy::Off` never throttles, so `DaThrottle` publishes the upper limits.
`ThrottleStrategy::Step` sets the intensity to `max_intensity` once the backlog reaches the
configured threshold. `ThrottleStrategy::Linear` grows intensity linearly from zero at the
threshold to `max_intensity` at twice the threshold and beyond. `ThrottleStrategy::Quadratic` grows
it between the same two points with the square of the backlog above the threshold, so it throttles
less than `ThrottleStrategy::Linear` in between.
`ThrottleParams` carries a fractional `intensity` value and the corresponding
`max_block_size` and `max_tx_size` byte limits computed by
interpolating between the upper and lower limits in `ThrottleConfig`.

`DaThrottle` turns the backlog into the `DaLimits` the block builders should apply and publishes
them on a `tokio::sync::watch` channel whenever they change. The driver calls `publish_limits`
with the DA backlog on every iteration and never waits on the block builders. Pushing the limits
over `miner_setMaxDASize` is up to the subscribers, which `base-batcher-service` provides.

This crate does not perform frame or blob encoding — those are handled by `base-batcher-encoder`
and `base-blobs`. It does not implement L2 block sourcing or L1 head tracking — those come from
`base-batcher-source`. Transaction signing, gas estimation, and confirmation polling belong to
`base-tx-manager`. Service configuration and process startup live in `base-batcher-service`.

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
