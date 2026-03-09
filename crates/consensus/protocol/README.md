# `base-protocol`

Protocol types for Base.

## Overview

Defines the core protocol types shared across Base's consensus, derivation, and proof layers. Includes
batch formats (`SingleBatch`, `SpanBatch`), frame and channel encoding, L1/L2 block reference types
(`BlockInfo`, `L2BlockInfo`), deposit decoding, payload attributes, output root computation, and
L1 block info structs for each hardfork (Bedrock through Jovian).

These types form the shared vocabulary between the derivation pipeline, consensus node, and proof
system.

## Usage

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
base-protocol = { workspace = true }
```

```rust,ignore
use base_protocol::{BatchType, BlockInfo, L2BlockInfo, OutputRoot};
```

## License

Licensed under the [MIT License](https://github.com/base/base/blob/main/LICENSE).
