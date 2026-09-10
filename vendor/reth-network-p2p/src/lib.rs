//! Provides abstractions and commonly used types for p2p.
//!
//! ## Feature Flags
//!
//! - `test-utils`: Export utilities for testing
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Traits for implementing P2P block body clients.
pub mod bodies;

/// An implementation that uses headers and bodies traits to download full blocks
pub mod full_block;
pub use full_block::{FullBlockClient, NoopFullBlockClient};

/// Traits for implementing P2P Header Clients. Also includes implementations
/// of a Linear and a Parallel downloader generic over the [`Consensus`] and
/// [`HeadersClient`].
///
/// [`Consensus`]: base_execution_evm_blocks::Consensus
/// [`HeadersClient`]: base_execution_network_wire::HeadersClient
pub mod headers;

/// Error types broadly used by p2p interfaces for any operation which may produce an error when
/// interacting with the network implementation
pub mod error;

/// Common test helpers for mocking out Consensus, Downloaders and Header Clients.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

pub use base_execution_network_wire::BlockAccessLists;

mod noop_snap;
