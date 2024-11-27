//! Contains the ratio compressor for Optimism.
//!
//! This is a port of the [RatioCompressor][rc] from the op-batcher.
//!
//! [rc]: https://github.com/ethereum-optimism/optimism/blob/develop/op-batcher/compressor/ratio_compressor.go#L7

/// The ratio compressor.
#[derive(Debug, Clone)]
pub struct RatioCompressor;
