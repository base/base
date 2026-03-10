# `base-alloy-flashblocks`

Flashblocks primitive types for Base's pre-confirmation infrastructure.

## Overview

Provides the core data structures for flashblock payloads, metadata, and delta encoding used in
Base's pre-confirmation system. Flashblocks are sub-block updates broadcast by the builder before
a full block is sealed. This crate defines the wire format for those updates, including
`FlashblocksPayloadV1` (the payload envelope), `FlashblocksDiff` (incremental state delta), and
`FlashblocksMetadata` (builder metadata).

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-alloy-flashblocks = { workspace = true }
```

```rust,ignore
use base_alloy_flashblocks::{FlashblocksPayloadV1, FlashblocksDiff};

let payload: FlashblocksPayloadV1 = serde_json::from_str(&msg)?;
let diff = payload.diff;
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
