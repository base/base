# base-batcher-encoder

Batcher encoding pipeline: `BatchPipeline` trait and `BatchEncoder` state machine.

The encoder is a synchronous, pure state machine that transforms L2 blocks into
L1 submission frames. No async, no I/O, no tokio dependency.

## Batch format

The encoder produces Single batches. Span decoding and derivation remain in the
protocol and consensus crates so nodes can continue deriving existing Span data.

Channels live in one append-only FIFO. At most the tail channel accepts new
Single-batch RLP values. Each accepted batch is written exactly once into its
incremental `CompressionStream`; decoder, assembled-channel, and frame-number
limits are checked before the stream is mutated.
`compressed_size_target`, when configured, is a soft operational limit:
the batch that reaches it remains in the channel, then the channel closes.
There is no shadow compressor, rollback path, or repeated whole-channel
compression.

The compressor transfers stable output without flushing or resetting its
dictionary. `DaEgress` frames that output directly against remaining blob
capacity. A full blob can therefore be emitted while its channel remains open.
Frames emitted before closure have `is_last = false`; closing finishes the
compressor, and the final frame has `is_last = true`.

Blob construction crosses channel boundaries in FIFO order without splitting or
renumbering an existing frame. Once built, each artifact is immutable and keeps
a stable identity through submission, retry, confirmation, safe-head pruning,
and replay.

A soft-target or protocol-limit close retains its partial tail so the next
channel can fill the blob. Reaching the channel's operational L1-block timeout
releases a partial artifact. An administrative flush closes the writable tail
and releases every retained partial artifact. Queue emptiness never releases a
partial blob. `max_blobs_per_tx` only controls transaction grouping.

`BatchComposer` owns Single-batch composition, `ChannelRecord` owns one
compression stream and its FIFO output, and `DaEgress` owns framing and immutable
artifacts. `BatchEncoder` coordinates those components with block, timeout,
confirmation, retry, and derivation-reconciliation state.

Channel-close metric `reason` labels: `soft_target`, `protocol_limit`,
`timeout`, `flush`, `discard`.

## Usage

```rust,ignore
use base_batcher_encoder::{
    BatchEncoder, BatchPipeline, DerivationReconciliation, EncoderConfig, StepResult,
    SubmissionPayload,
};

let mut encoder = BatchEncoder::new(rollup_config, EncoderConfig::default())?;

// Feed L2 blocks.
encoder.add_block(block)?;

// Step until idle.
loop {
    match encoder.step()? {
        StepResult::Idle => break,
        _ => {}
    }
}

// Drain ready submissions.
while let Some(sub) = encoder.next_submission() {
    match sub.payload() {
        SubmissionPayload::Blobs(blobs) => {
            for blob in blobs {
                let encoded = base_blobs::BlobEncoder::encode_packed(blob.frames())?;
            }
        }
        SubmissionPayload::Calldata(frame) => {
            // The driver encodes this frame with FrameEncoder::to_calldata.
        }
    }
    encoder.confirm(sub.id, l1_block_number);
    encoder.advance_l1_head(l1_block_number);
    // Call encoder.requeue(sub.id) if submission fails and frames must be retried.
}

// Reconcile derivation progress. `current_l1_number` is `None` when no cursor is available.
match encoder.reconcile_derivation(safe_head, current_l1_number) {
    DerivationReconciliation::Consistent => {}
    DerivationReconciliation::SafeHeadMismatch
    | DerivationReconciliation::StalledChannel => {
        encoder.reset();
    }
}
```

## Confirm / requeue lifecycle

During normal operation, every submission drained from `next_submission()` **must**
be resolved with either `confirm(id, l1_block)` or `requeue(id)`:

- `confirm` records immutable artifact inclusion. Completed channels and their L2 blocks remain
  buffered until `reconcile_derivation` observes the corresponding safe-head advance.
- `requeue` makes the exact same immutable artifacts available again. Use this
  when an L1 transaction fails or is dropped.

Failing to call either leaves its artifacts pending in the egress ledger.
The block deque tracks the `(safe, unsafe]` range; normal removal happens during
derivation reconciliation.

## Frame encoding

`FrameEncoder::to_calldata(frame)` produces the exact byte sequence the derivation
pipeline expects: `[DERIVATION_VERSION_0] ++ frame.encode()`. Both `BatchDriver` (the
production async driver in `base-batcher-core`) and the action-test `Batcher` harness use
this shared implementation so the framing logic is defined exactly once.

For EIP-4844 blob submission, encode each validated blob payload with
`base_blobs::BlobEncoder::encode_packed(blob.frames())`.
