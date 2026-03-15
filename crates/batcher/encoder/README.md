# base-batcher-encoder

Batcher encoding pipeline: `BatchPipeline` trait and `BatchEncoder` state machine.

The encoder is a synchronous, pure state machine that transforms L2 blocks into
L1 submission frames. No async, no I/O, no tokio dependency.

## Usage

```rust,ignore
use base_batcher_encoder::{BatchEncoder, EncoderConfig, FrameEncoder, StepResult};

let mut encoder = BatchEncoder::new(rollup_config, EncoderConfig::default());

// Feed L2 blocks.
encoder.add_block(block)?;

// Step until idle.
loop {
    match encoder.step() {
        StepResult::Idle => break,
        _ => {}
    }
}

// Drain ready submissions.
while let Some(sub) = encoder.next_submission() {
    for frame in &sub.frames {
        // Encode as L1 calldata:
        let calldata = FrameEncoder::to_calldata(frame);
        // Or pack into EIP-4844 blobs via base-blobs::BlobEncoder.
    }
    encoder.confirm(sub.id, l1_block_number);
    // Call encoder.requeue(sub.id) if submission fails and frames must be retried.
}
```

## Confirm / requeue lifecycle

Every submission drained from `next_submission()` **must** be resolved with either
`confirm(id, l1_block)` or `requeue(id)`:

- `confirm` prunes the submission's frames from the channel's pending set. Once all
  frames of a channel are confirmed, the channel is finalized and its L2 blocks are
  removed from the encoder's input queue, keeping memory bounded.
- `requeue` rewinds the channel's frame cursor so the same frames are re-emitted on the
  next `next_submission()` call. Use this when an L1 transaction fails or is dropped.

Failing to call either will cause the encoder's internal `pending` map and block deque
to grow without bound.

## Frame encoding

`FrameEncoder::to_calldata(frame)` produces the exact byte sequence the derivation
pipeline expects: `[DERIVATION_VERSION_0] ++ frame.encode()`. Both `BatchDriver` (the
production async driver in `base-batcher-core`) and the action-test `Batcher` harness use
this shared implementation so the framing logic is defined exactly once.

For EIP-4844 blob submission, use `base_blobs::BlobEncoder::encode_frames(&frames)`.
