#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

use proc_macro::TokenStream;

mod arbitrary;
mod compact;
mod expansion;
use expansion::ZstdConfig;

/// Derives compact storage encoding with field-presence flags.
#[proc_macro_derive(Compact, attributes(maybe_zero, reth_codecs))]
pub fn derive(input: TokenStream) -> TokenStream {
    expansion::derive(input)
}

/// Derives compact storage encoding with the supplied compression dictionary.
#[proc_macro_derive(CompactZstd, attributes(maybe_zero, reth_codecs, reth_zstd))]
pub fn derive_zstd(input: TokenStream) -> TokenStream {
    expansion::derive_zstd(input)
}

/// Adds arbitrary round-trip tests to a type.
#[proc_macro_attribute]
pub fn add_arbitrary_tests(args: TokenStream, input: TokenStream) -> TokenStream {
    expansion::add_arbitrary_tests(args, input)
}

/// Generates codec round-trip tests for a type and named test module.
#[proc_macro]
pub fn generate_tests(input: TokenStream) -> TokenStream {
    expansion::generate_tests(input)
}
