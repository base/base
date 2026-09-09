# `base-consensus-batch-types`

Protocol types for Base.

## Overview

Defines the core protocol types shared across Base's consensus, derivation, and proof layers. Includes
singular batches (`SingleBatch`), frame and channel encoding, L1/L2 block reference types
(`BlockInfo`, `L2BlockInfo`), deposit decoding, payload attributes, output root computation, and
L1 block info structs for each upgrade (Bedrock through Jovian).

These types form the shared vocabulary between the derivation pipeline, consensus node, and proof
system.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-consensus-batch-types = { workspace = true }
```

```rust,ignore
use base_consensus_batch_types::{BatchType, BlockInfo, L2BlockInfo, OutputRoot};
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
