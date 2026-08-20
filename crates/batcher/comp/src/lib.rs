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

mod types;
pub use types::{CompressionAlgo, CompressionError};

#[cfg(feature = "std")]
mod brotli;
#[cfg(feature = "std")]
pub use brotli::BrotliCompressor;

#[cfg(feature = "std")]
mod stream;
#[cfg(feature = "std")]
pub use stream::{CompressionBackend, CompressionStream};
