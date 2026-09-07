//! Support for configuring the components of a node.
//!
//! Customizable components of the node include:
//!  - The transaction pool.
//!  - The network implementation.
//!  - The payload builder service.
//!
//! Components depend on a fully type configured node: [`crate::FullNodeTypes`].

use std::fmt::Debug;

use reth_evm::BaseEvmConfig;
use reth_payload_builder::PayloadBuilderHandle;
use reth_transaction_pool::TransactionPool;

mod builder;
pub use builder::{BuiltComponents, NodeComponentsBuilder};

mod pool;
pub use pool::*;

/// All the components of the node.
///
/// This provides access to all the components of the node.
#[derive(Debug)]
pub struct Components<Network, Pool, Consensus> {
    /// The transaction pool of the node.
    pub transaction_pool: Pool,
    /// The node's EVM configuration, defining settings for the Ethereum Virtual Machine.
    pub evm_config: BaseEvmConfig,
    /// The consensus implementation of the node.
    pub consensus: Consensus,
    /// The network implementation of the node.
    pub network: Network,
    /// The handle to the payload builder service.
    pub payload_builder_handle: PayloadBuilderHandle,
}

impl<N, Pool, Cons> Clone for Components<N, Pool, Cons>
where
    N: Clone,
    Pool: TransactionPool,
    Cons: Clone,
{
    fn clone(&self) -> Self {
        Self {
            transaction_pool: self.transaction_pool.clone(),
            evm_config: self.evm_config.clone(),
            consensus: self.consensus.clone(),
            network: self.network.clone(),
            payload_builder_handle: self.payload_builder_handle.clone(),
        }
    }
}
