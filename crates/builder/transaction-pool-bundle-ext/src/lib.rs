//! Crate facilitating simple extension of the reth `TransationPool` with
//! arbitrary bundle support.
//!
//! Contains
//! - A reth `TransactionPool` trait extension ([`TransactionPoolBundleExt`])
//!   allowing bundle support to be added into reth nodes.
//!
//! - A [`TransactionPoolBundleExt`] implementation [`BundleSupportedPool`],
//!   which encapsulates the reth `TransactionPool` and a generic implementation
//!   of the [`BundlePoolOperations`] trait.
//!
//! ## Usage
//!
//! 1. When implementing `PoolBuilder<Node>` on your node, pass a custom `type Pool ...`
//!    that is a [`BundleSupportedPool`] type. Your [`BundleSupportedPool`] type
//!    must specify your chosen [`BundlePoolOperations`] implementation, which you
//!    can initialise inside `fn build_pool` when you build the pool.
//! 2. Whereever you require access to bundles, e.g. when implementing RPCs or
//!    `PayloadServiceBuilder<Node, Pool>` on your node, modify the `Pool` trait bound
//!    replacing `TransactionPool` with [`TransactionPoolBundleExt`]. This allows access to
//!    [`BundlePoolOperations`] methods almost everywhere in the node.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod bundle_supported_pool;
mod traits;
pub use bundle_supported_pool::BundleSupportedPool;
pub use traits::{BundlePoolOperations, TransactionPoolBundleExt};
