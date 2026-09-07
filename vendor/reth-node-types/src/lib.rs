//! Standalone crate for Reth configuration traits and builder types.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

use core::{fmt::Debug, marker::PhantomData};

use reth_db_api::{Database, database_metrics::DatabaseMetrics};
pub use reth_primitives_traits::{Block, BlockBody, FullBlock, FullReceipt, FullSignedTx};

/// Database backend used by node providers.
///
/// Its types are configured by node internally and are not intended to be user configurable.
pub trait NodeTypesWithDB: Clone + Debug + Send + Sync + Unpin + 'static {
    /// Underlying database type used by the node to store and retrieve data.
    type DB: Database + DatabaseMetrics + Clone + Unpin + 'static;
}

/// Selects the database backend used by node providers.
#[derive(Clone, Debug, Default)]
pub struct NodeTypesWithDBAdapter<DB> {
    db: PhantomData<DB>,
}

impl<DB> NodeTypesWithDBAdapter<DB> {
    /// Create a new adapter with the configured types.
    pub fn new() -> Self {
        Self { db: Default::default() }
    }
}

impl<DB> NodeTypesWithDB for NodeTypesWithDBAdapter<DB>
where
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
{
    type DB = DB;
}
