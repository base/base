# `base-protocol`

Protocol types for Base.

## Overview

Defines the core protocol types shared across Base's consensus, derivation, and proof layers. Includes
batch formats (`SingleBatch`, `SpanBatch`), frame and channel encoding, L1/L2 block reference types
(`BlockInfo`, `L2BlockInfo`), deposit decoding, payload attributes, output root computation, and
L1 block info structs for each upgrade (Bedrock through Jovian).

These types form the shared vocabulary between the derivation pipeline, consensus node, and proof
system.

## Denim L1 origin rule

After Denim activation, consecutive L2 blocks with the same whole-second header timestamp must
reference the same L1 origin (both number and hash). An origin change is allowed only when the
whole-second timestamp advances, subject to the existing origin and timestamp checks. This keeps
each second's EIP-4788 beacon-root value stable across its 200ms blocks.

The rule applies to sequenced blocks, submitted single batches, and forced-empty derivation. Empty
blocks retain the current origin until the next second even when maximum sequencer drift is
exceeded; sequenced user transactions remain forbidden past drift. At the next second, the existing
origin-advancement rules resume. Pre-Denim behavior is unchanged.

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
