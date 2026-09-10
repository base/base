#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "tracy-allocator")]
use base_common_observability_tracing as _;
#[cfg(feature = "tracy-allocator")]
use tracy_client as _;

mod allocator;
#[cfg(all(feature = "jemalloc", unix))]
pub use allocator::tikv_jemalloc_sys;
pub use allocator::{Allocator, new_allocator};
mod cancellation;
pub use cancellation::{CancellationGuard, CancellationToken};

/// Helper function to load a secret key from a file.
mod load_secret_key;
pub use load_secret_key::{
    SecretKeyError, get_secret_key, parse_secret_key_from_hex, rng_secret_key,
};

/// Cli parsers functions.
mod parsers;
pub use parsers::{
    SocketAddressParsingError, format_duration_as_secs_or_ms, hash_or_num_value_parser,
    parse_duration_from_secs, parse_duration_from_secs_or_ms, parse_ether_value,
    parse_socket_address, read_json_from_file,
};
