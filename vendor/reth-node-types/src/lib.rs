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
use reth_payload_primitives::PayloadTypes;
pub use reth_primitives_traits::{Block, BlockBody, FullBlock, FullReceipt, FullSignedTx};

/// The type that configures the essential types of an Ethereum-like node.
///
/// This includes the primitive types of a node and chain specification.
///
/// This trait is intended to be stateless and only define the types of the node.
pub trait NodeTypes: Clone + Debug + Send + Sync + Unpin + 'static {
    /// The type used for configuration of the EVM.
    type ChainSpec: EthChainSpec<Header = alloy_consensus::Header>;
    /// The type responsible for writing chain primitives to storage.
    type Storage: Default + Send + Sync + Unpin + Debug + 'static;
    /// The node's engine types, defining the interaction with the consensus engine.
    type Payload: PayloadTypes;
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
    type Storage = Types::Storage;
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
pub struct AnyNodeTypes<C = (), S = (), PL = ()>(PhantomData<C>, PhantomData<S>, PhantomData<PL>);

impl<C, S, PL> AnyNodeTypes<C, S, PL> {
    /// Creates a new instance of [`AnyNodeTypes`].
    pub const fn new() -> Self {
        Self(PhantomData, PhantomData, PhantomData)
    }

    /// Sets the `ChainSpec` associated type.
    pub const fn chain_spec<T>(self) -> AnyNodeTypes<T, S, PL> {
        AnyNodeTypes::new()
    }

    /// Sets the `Storage` associated type.
    pub const fn storage<T>(self) -> AnyNodeTypes<C, T, PL> {
        AnyNodeTypes::new()
    }

    /// Sets the `Payload` associated type.
    pub const fn payload<T>(self) -> AnyNodeTypes<C, S, T> {
        AnyNodeTypes::new()
    }
}

impl<C, S, PL> NodeTypes for AnyNodeTypes<C, S, PL>
where
    C: EthChainSpec<Header = alloy_consensus::Header> + Clone + 'static,
    S: Default + Clone + Send + Sync + Unpin + Debug + 'static,
    PL: PayloadTypes + Send + Sync + Unpin + 'static,
{
    type ChainSpec = C;
    type Storage = S;
    type Payload = PL;
}

/// A [`NodeTypes`] type builder.
#[derive(Clone, Debug, Default)]
pub struct AnyNodeTypesWithEngine<E = (), C = (), S = (), PL = ()> {
    /// Embedding the basic node types.
    _base: AnyNodeTypes<C, S, PL>,
    /// Phantom data for the engine.
    _engine: PhantomData<E>,
}

impl<E, C, S, PL> AnyNodeTypesWithEngine<E, C, S, PL> {
    /// Creates a new instance of [`AnyNodeTypesWithEngine`].
    pub const fn new() -> Self {
        Self { _base: AnyNodeTypes::new(), _engine: PhantomData }
    }

    /// Sets the `Engine` associated type.
    pub const fn engine<T>(self) -> AnyNodeTypesWithEngine<T, C, S, PL> {
        AnyNodeTypesWithEngine::new()
    }

    /// Sets the `ChainSpec` associated type.
    pub const fn chain_spec<T>(self) -> AnyNodeTypesWithEngine<E, T, S, PL> {
        AnyNodeTypesWithEngine::new()
    }

    /// Sets the `Storage` associated type.
    pub const fn storage<T>(self) -> AnyNodeTypesWithEngine<E, C, T, PL> {
        AnyNodeTypesWithEngine::new()
    }

    /// Sets the `Payload` associated type.
    pub const fn payload<T>(self) -> AnyNodeTypesWithEngine<E, C, S, T> {
        AnyNodeTypesWithEngine::new()
    }
}

impl<E, C, S, PL> NodeTypes for AnyNodeTypesWithEngine<E, C, S, PL>
where
    E: EngineTypes + Send + Sync + Unpin,
    C: EthChainSpec<Header = alloy_consensus::Header> + Clone + 'static,
    S: Default + Clone + Send + Sync + Unpin + Debug + 'static,
    PL: PayloadTypes + Send + Sync + Unpin + 'static,
{
    type ChainSpec = C;
    type Storage = S;
    type Payload = PL;
}

/// Helper adapter type for accessing [`PayloadTypes::PayloadAttributes`] on [`NodeTypes`].
pub type PayloadAttrTy<N> = <<N as NodeTypes>::Payload as PayloadTypes>::PayloadAttributes;
