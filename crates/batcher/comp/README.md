# `base-comp`

Compression types for Base.

## Overview

Provides channel encoding and compression utilities for the Base derivation pipeline. `ChannelOut`
encodes batches into compressed frames using a pluggable `VariantCompressor` (Brotli or zlib).
`CompressionStream` compresses a channel incrementally; `CompressionAlgo::compress_channel`
compresses one complete channel in a single call. The `MockCompressor` is available under the
`test-utils` feature for deterministic testing.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-comp = { workspace = true }
```

## Batch to Frames Example

The following example demonstrates encoding a [`SingleBatch`] through a
[`ChannelOut`] and into individual [`Frame`]s.

```rust,no_run
use std::sync::Arc;

use alloy_primitives::BlockHash;
use base_comp::{ChannelOut, CompressionAlgo, VariantCompressor};
use base_common_genesis::RollupConfig;
use base_protocol::{ChannelId, SingleBatch};

// Use the example transaction
let transactions = vec![];

// Construct a basic `SingleBatch`
let parent_hash = BlockHash::ZERO;
let epoch_num = 1;
let epoch_hash = BlockHash::ZERO;
let timestamp = 1;
let single_batch = SingleBatch { parent_hash, epoch_num, epoch_hash, timestamp, transactions };

// Create a new channel.
let id = ChannelId::default();
let config = Arc::new(RollupConfig::default());
let compressor: VariantCompressor = CompressionAlgo::Brotli10.into();
let mut channel_out = ChannelOut::new(id, config, compressor);

// Encode and compress the batch into the channel.
channel_out.add_single_batch(single_batch).unwrap();

// Finalize and output frames
for frame in channel_out.into_frames(100).expect("outputs frames") {
    println!("Frame: {}", alloy_primitives::hex::encode(frame.encode()));
}
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
