//! Contains the shadow compressor for Optimism.
//!
//! This is a port of the [ShadowCompressor][sc] from the op-batcher.
//!
//! [sc]: https://github.com/ethereum-optimism/optimism/blob/develop/op-batcher/compressor/shadow_compressor.go#L18

/// The shadow compressor.
#[derive(Debug, Clone)]
pub struct ShadowCompressor;
