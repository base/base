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
use reth_engine_primitives::EngineTypes;
pub use reth_primitives_traits::{Block, BlockBody, FullBlock, FullReceipt, FullSignedTx};

/// The type that configures the essential types of an Ethereum-like node.
///
/// This includes the primitive types of a node and chain specification.
///
/// This trait is intended to be stateless and only define the types of the node.
pub trait NodeTypes: Clone + Debug + Send + Sync + Unpin + 'static {
    /// The type used for configuration of the EVM.
    type ChainSpec: EthChainSpec<Header = alloy_consensus::Header>;
    /// The node's engine types, defining the interaction with the consensus engine.
    type Payload: EngineTypes;
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
    type Payload = Types::Payload;
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
pub struct AnyNodeTypes<C = (), PL = ()>(PhantomData<C>, PhantomData<PL>);

impl<C, PL> AnyNodeTypes<C, PL> {
    /// Creates a new instance of [`AnyNodeTypes`].
    pub const fn new() -> Self {
        Self(PhantomData, PhantomData)
    }

    /// Sets the `ChainSpec` associated type.
    pub const fn chain_spec<T>(self) -> AnyNodeTypes<T, PL> {
        AnyNodeTypes::new()
    }

    /// Sets the `Payload` associated type.
    pub const fn payload<T>(self) -> AnyNodeTypes<C, T> {
        AnyNodeTypes::new()
    }
}

impl<C, PL> NodeTypes for AnyNodeTypes<C, PL>
where
    C: EthChainSpec<Header = alloy_consensus::Header> + Clone + 'static,
    PL: EngineTypes + Send + Sync + Unpin + 'static,
{
    type ChainSpec = C;
    type Payload = PL;
}

/// A [`NodeTypes`] type builder.
#[derive(Clone, Debug, Default)]
pub struct AnyNodeTypesWithEngine<E = (), C = (), PL = ()> {
    /// Embedding the basic node types.
    _base: AnyNodeTypes<C, PL>,
    /// Phantom data for the engine.
    _engine: PhantomData<E>,
}

impl<E, C, PL> AnyNodeTypesWithEngine<E, C, PL> {
    /// Creates a new instance of [`AnyNodeTypesWithEngine`].
    pub const fn new() -> Self {
        Self { _base: AnyNodeTypes::new(), _engine: PhantomData }
    }

    /// Sets the `Engine` associated type.
    pub const fn engine<T>(self) -> AnyNodeTypesWithEngine<T, C, PL> {
        AnyNodeTypesWithEngine::new()
    }

    /// Sets the `ChainSpec` associated type.
    pub const fn chain_spec<T>(self) -> AnyNodeTypesWithEngine<E, T, PL> {
        AnyNodeTypesWithEngine::new()
    }

    /// Sets the `Payload` associated type.
    pub const fn payload<T>(self) -> AnyNodeTypesWithEngine<E, C, T> {
        AnyNodeTypesWithEngine::new()
    }
}

impl<E, C, PL> NodeTypes for AnyNodeTypesWithEngine<E, C, PL>
where
    E: EngineTypes + Send + Sync + Unpin,
    C: EthChainSpec<Header = alloy_consensus::Header> + Clone + 'static,
    PL: EngineTypes + Send + Sync + Unpin + 'static,
{
    type ChainSpec = C;
    type Payload = PL;
}
