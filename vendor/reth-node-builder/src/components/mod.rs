//! Base pool, consensus, networking, and payload-service components.

use std::sync::Arc;

use base_execution_consensus::BaseBeaconConsensus;
use base_execution_evm::BaseEvmConfig;
use base_execution_payload_builder::PayloadBuilderHandle;
use base_execution_txpool::BaseTransactionPool;
use reth_db_api::{Database, database_metrics::DatabaseMetrics};
use reth_network::NetworkHandle;
use reth_provider::providers::BlockchainProvider;
use reth_transaction_pool::blobstore::DiskFileBlobStore;

mod builder;
pub use builder::ComponentBuilder;

mod pool;
pub use pool::*;

/// All the components of the node.
///
/// This provides access to all the components of the node.
#[derive(Debug)]
pub struct Components<DB: Database + DatabaseMetrics + Clone + Unpin + 'static> {
    /// The transaction pool of the node.
    pub transaction_pool: BaseTransactionPool<BlockchainProvider<DB>, DiskFileBlobStore>,
    /// The node's EVM configuration, defining settings for the Ethereum Virtual Machine.
    pub evm_config: BaseEvmConfig,
    /// The consensus implementation of the node.
    pub consensus: Arc<BaseBeaconConsensus>,
    /// The network implementation of the node.
    pub network: NetworkHandle,
    /// The handle to the payload builder service.
    pub payload_builder_handle: PayloadBuilderHandle,
}

impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static> Clone for Components<DB> {
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
