#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod channel_out;
pub use channel_out::{ChannelOut, ChannelOutError};

mod composer;
pub use composer::{BatchComposeError, BatchComposer};

mod traits;
pub use traits::{ChannelCompressor, CompressorWriter};

mod config;
pub use config::Config;

mod types;
pub use types::{CompressionAlgo, CompressorError, CompressorResult, CompressorType};

mod zlib;
pub use zlib::{ZlibCompressor, compress_zlib, decompress_zlib};

#[cfg(feature = "std")]
mod brotli;
#[cfg(feature = "std")]
pub use brotli::{BrotliCompressionError, BrotliCompressor, BrotliLevel, compress_brotli};

#[cfg(feature = "std")]
mod variant;
#[cfg(feature = "std")]
pub use variant::VariantCompressor;

#[cfg(feature = "std")]
mod shadow;
#[cfg(feature = "std")]
pub use shadow::ShadowCompressor;

#[cfg(feature = "std")]
mod ratio;
#[cfg(feature = "std")]
pub use ratio::RatioCompressor;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
