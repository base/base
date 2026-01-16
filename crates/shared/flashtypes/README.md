# `base-flashtypes`

<a href="https://github.com/base/base/actions/workflows/ci.yml"><img src="https://github.com/base/base/actions/workflows/ci.yml/badge.svg?label=ci" alt="CI"></a>
<a href="https://github.com/base/base/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-MIT-d1d1f6.svg?label=license&labelColor=2a2f35" alt="MIT License"></a>

Low-level primitive types for flashblocks. Provides the core data structures for flashblock payloads, metadata, and delta encoding used in Base's pre-confirmation infrastructure.

## Overview

- **`Flashblock`**: Core flashblock type containing payload and metadata.
- **`Metadata`**: Flashblock metadata including index, base fee, and builder info.
- **`FlashblocksPayloadV1`**: Complete flashblocks payload containing base execution payload and deltas.
- **`ExecutionPayloadBaseV1`**: Base execution payload shared across flashblock deltas.
- **`ExecutionPayloadFlashblockDeltaV1`**: Delta-encoded flashblock containing incremental transaction updates.
- **`FlashblockDecodeError`**: Error type for flashblock decoding failures.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-flashtypes = { git = "https://github.com/base/base" }
```

Parse and work with flashblock payloads:

```rust,ignore
use base_flashtypes::{Flashblock, FlashblocksPayloadV1, Metadata};

// Deserialize a flashblocks payload from JSON
let payload: FlashblocksPayloadV1 = serde_json::from_str(json)?;

// Access base payload and deltas
let base = &payload.base;
let deltas = &payload.diffs;

// Work with individual flashblocks
for delta in deltas {
    let transactions = &delta.transactions;
    let index = delta.index;
}
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
