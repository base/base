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

use reth_chainspec::EthChainSpec;
use reth_db_api::{Database, database_metrics::DatabaseMetrics};
pub use reth_primitives_traits::{Block, BlockBody, FullBlock, FullReceipt, FullSignedTx};

/// The type that configures the essential types of an Ethereum-like node.
///
/// This includes the primitive types of a node and chain specification.
///
/// This trait is intended to be stateless and only define the types of the node.
pub trait NodeTypes: Clone + Debug + Send + Sync + Unpin + 'static {
    /// The type used for configuration of the EVM.
    type ChainSpec: EthChainSpec<Header = alloy_consensus::Header>;
}

/// A helper trait that is downstream of the [`NodeTypes`] trait and adds database to the
/// node.
///
/// Its types are configured by node internally and are not intended to be user configurable.
pub trait NodeTypesWithDB: NodeTypes {
    /// Underlying database type used by the node to store and retrieve data.
    type DB: Database + DatabaseMetrics + Clone + Unpin + 'static;
}

/// An adapter type combining [`NodeTypes`] and db into [`NodeTypesWithDB`].
#[derive(Clone, Debug, Default)]
pub struct NodeTypesWithDBAdapter<Types, DB> {
    types: PhantomData<Types>,
    db: PhantomData<DB>,
}

impl<Types, DB> NodeTypesWithDBAdapter<Types, DB> {
    /// Create a new adapter with the configured types.
    pub fn new() -> Self {
        Self { types: Default::default(), db: Default::default() }
    }
}

impl<Types, DB> NodeTypes for NodeTypesWithDBAdapter<Types, DB>
where
    Types: NodeTypes,
    DB: Clone + Debug + Send + Sync + Unpin + 'static,
{
    type ChainSpec = Types::ChainSpec;
}

impl<Types, DB> NodeTypesWithDB for NodeTypesWithDBAdapter<Types, DB>
where
    Types: NodeTypes,
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
{
    type DB = DB;
}

/// A [`NodeTypes`] type builder.
#[derive(Clone, Debug, Default)]
pub struct AnyNodeTypes<C = ()>(PhantomData<C>);

impl<C> AnyNodeTypes<C> {
    /// Creates a new instance of [`AnyNodeTypes`].
    pub const fn new() -> Self {
        Self(PhantomData)
    }

    /// Sets the `ChainSpec` associated type.
    pub const fn chain_spec<T>(self) -> AnyNodeTypes<T> {
        AnyNodeTypes::new()
    }
}

impl<C> NodeTypes for AnyNodeTypes<C>
where
    C: EthChainSpec<Header = alloy_consensus::Header> + Clone + 'static,
{
    type ChainSpec = C;
}
