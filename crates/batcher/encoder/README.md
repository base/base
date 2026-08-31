# base-batcher-encoder

Batcher encoding pipeline: `BatchPipeline` trait and `BatchEncoder` state machine.

The encoder is a synchronous, pure state machine that transforms L2 blocks into
L1 submission frames. No async, no I/O, no tokio dependency.

## Batch format

The encoder produces Single batches. Span decoding and derivation remain in the
protocol and consensus crates so nodes can continue deriving existing Span data.

## Usage

```rust,ignore
use base_batcher_encoder::{
    BatchEncoder, BatchPipeline, DerivationReconciliation, EncoderConfig, FrameEncoder,
    StepResult, SubmissionPayload,
};

let mut encoder = BatchEncoder::new(rollup_config, EncoderConfig::default());

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
                let _ = base_blobs::BlobEncoder::encode_packed(blob.frames());
            }
        }
        SubmissionPayload::Calldata(frame) => {
            let _ = FrameEncoder::to_calldata(frame);
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

- `confirm` records frame inclusion. Completed channels and their L2 blocks remain
  buffered until `reconcile_derivation` observes the corresponding safe-head advance.
- `requeue` makes the submission's frames available again. Use this when an L1
  transaction fails or is dropped.

Failing to call either leaves the submission in the encoder's internal `pending` map.
The block deque tracks the `(safe, unsafe]` range; normal removal happens during
derivation reconciliation.

## Frame encoding

`FrameEncoder::to_calldata(frame)` produces the exact byte sequence the derivation
pipeline expects: `[DERIVATION_VERSION_0] ++ frame.encode()`. Both `BatchDriver` (the
production async driver in `base-batcher-core`) and the action-test `Batcher` harness use
this shared implementation so the framing logic is defined exactly once.

For EIP-4844 blob submission, use `base_blobs::BlobEncoder::encode_packed(&frames)`.
