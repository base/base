# `base-common-flashblocks`

Flashblocks primitive types for Base's pre-confirmation infrastructure.

## Overview

Provides the core data structures for flashblock payloads, metadata, and delta encoding used in
Base's pre-confirmation system. Flashblocks are sub-block updates broadcast by the builder before
a full block is sealed. `FlashblocksPayloadV1` is the wire payload envelope. Its `diff` field is an
`ExecutionPayloadFlashblockDeltaV1` containing updated execution fields and new transactions.
The envelope's `metadata` field is a `serde_json::Value`. `Metadata` is the parsed metadata type
used by `Flashblock`, containing the block number and previous flashblock identifier.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-common-flashblocks = { workspace = true }
```

```rust,ignore
use base_common_flashblocks::{ExecutionPayloadFlashblockDeltaV1, FlashblocksPayloadV1};

let payload: FlashblocksPayloadV1 = serde_json::from_str(&msg)?;
let diff: ExecutionPayloadFlashblockDeltaV1 = payload.diff;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
