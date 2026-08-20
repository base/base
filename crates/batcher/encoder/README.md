# base-batcher-encoder

Synchronous encoder: L2 blocks → L1 frames. No async, no I/O.

Produces Single batches. Span decoding stays in protocol/consensus for
historical derivation.

The writable channel is the FIFO tail. Each accepted batch is compressed once;
hard limits are checked before the stream is mutated. `compressed_size_target`
is optional and closes after the batch that reaches it.

`DaEgress` frames compressor output against remaining blob capacity, so a full
blob can emit while the channel is still open. Artifacts are immutable after
creation. A size or protocol-limit close keeps its partial tail for the next
channel; timeout and flush release it. `max_blobs_per_tx` only groups
transactions.

Channel-close metric `reason` labels: `soft_target`, `protocol_limit`,
`timeout`, `flush`, `discard`.

## Usage

```rust,ignore
use base_batcher_encoder::{
    BatchEncoder, BatchPipeline, DerivationReconciliation, EncoderConfig, StepResult,
    SubmissionPayload,
};

let mut encoder = BatchEncoder::new(rollup_config, EncoderConfig::default())?;

encoder.add_block(block)?;

loop {
    match encoder.step()? {
        StepResult::Idle => break,
        _ => {}
    }
}

while let Some(sub) = encoder.next_submission() {
    match sub.payload() {
        SubmissionPayload::Blobs(blobs) => {
            for blob in blobs {
                let encoded = base_blobs::BlobEncoder::encode_packed(blob.frames())?;
            }
        }
        SubmissionPayload::Calldata(frame) => {
            let _ = base_batcher_encoder::FrameEncoder::to_calldata(&frame);
        }
    }
    encoder.confirm(sub.id, l1_block_number);
    encoder.advance_l1_head(l1_block_number);
}

match encoder.reconcile_derivation(safe_head, current_l1_number) {
    DerivationReconciliation::Consistent => {}
    DerivationReconciliation::SafeHeadMismatch
    | DerivationReconciliation::StalledChannel => {
        encoder.reset();
    }
}
```

## Confirm / requeue

Every `next_submission()` must be followed by `confirm` or `requeue`.
`confirm` records inclusion; blocks stay until `reconcile_derivation`.
`requeue` puts the same artifacts back to ready.

`FrameEncoder::to_calldata` is `[DERIVATION_VERSION_0] ++ frame.encode()`.
Blob payloads use `base_blobs::BlobEncoder::encode_packed`.
