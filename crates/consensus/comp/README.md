# `base-comp`

Compression types for Base.

## Batch to Frames Example

The following example demonstrates encoding a [`SingleBatch`] through a
[`ChannelOut`] and into individual [`Frame`]s.

```rust,no_run
use alloy_primitives::BlockHash;
use base_comp::{ChannelOut, CompressionAlgo, VariantCompressor};
use base_consensus_genesis::RollupConfig;
use base_protocol::{Batch, ChannelId, SingleBatch};

// Use the example transaction
let transactions = vec![];

// Construct a basic `SingleBatch`
let parent_hash = BlockHash::ZERO;
let epoch_num = 1;
let epoch_hash = BlockHash::ZERO;
let timestamp = 1;
let single_batch = SingleBatch { parent_hash, epoch_num, epoch_hash, timestamp, transactions };
let batch = Batch::Single(single_batch);

// Create a new channel.
let id = ChannelId::default();
let config = RollupConfig::default();
let compressor: VariantCompressor = CompressionAlgo::Brotli10.into();
let mut channel_out = ChannelOut::new(id, &config, compressor);

// Add the compressed batch to the `ChannelOut`.
channel_out.add_batch(batch).unwrap();

// Output frames
while channel_out.ready_bytes() > 0 {
    let frame = channel_out.output_frame(100).expect("outputs frame");
    println!("Frame: {}", alloy_primitives::hex::encode(frame.encode()));
    if channel_out.ready_bytes() <= 100 {
        channel_out.close();
    }
}

assert!(channel_out.closed);
```

## Features

| Feature | Description |
|---------|-------------|
| `std` | Enables standard library support and Brotli compression |
| `serde` | Enables serialization support |
| `test-utils` | Exports [`MockCompressor`] for testing |
| `arbitrary` | Enables property-based testing support |

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
