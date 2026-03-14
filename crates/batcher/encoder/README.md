# base-batcher-encoder

Batcher encoding pipeline: `BatchPipeline` trait and `BatchEncoder` state machine.

The encoder is a synchronous, pure state machine that transforms L2 blocks into
L1 submission frames. No async, no I/O, no tokio dependency.

## Usage

```rust,ignore
use base_batcher_encoder::{BatchEncoder, EncoderConfig, StepResult};

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
    // Submit to L1...
    encoder.confirm(sub.id, l1_block_number);
}
```
