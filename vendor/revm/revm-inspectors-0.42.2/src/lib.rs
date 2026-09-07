//! revm [Inspector](revm::Inspector) implementations, such as call tracers
//!
//! ## Feature Flags
//!
//! - `js-tracer`: Enables a JavaScript tracer implementation. This pulls in extra dependencies
//!   (such as `boa`, `tokio` and `serde_json`).

#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![deny(unused_must_use, rust_2018_idioms)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

/// An inspector implementation for an EIP2930 Accesslist
pub mod access_list;

/// implementation of an opcode counter for the EVM.
pub mod opcode;

/// An inspector for recording traces
pub mod tracing;

/// An inspector for recording internal transfers.
pub mod transfer;

/// An inspector for tracking storage access.
pub mod storage;

pub use colorchoice::ColorChoice;
