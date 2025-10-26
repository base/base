use crate::postgres::{BlockInfo, BlockInfoUpdate, BundleFilter, BundleStats};
use alloy_primitives::TxHash;
use anyhow::Result;
use sqlx::types::chrono::{DateTime, Utc};
use uuid::Uuid;

use tips_common::{BundleState, BundleWithMetadata};

/// Trait defining the interface for bundle datastore operations
#[async_trait::async_trait]
pub trait BundleDatastore: Send + Sync {
    /// Insert a new bundle into the datastore
    async fn insert_bundle(&self, bundle: BundleWithMetadata) -> Result<Uuid>;

    /// Fetch a bundle with metadata by its ID
    async fn get_bundle(&self, id: Uuid) -> Result<Option<BundleWithMetadata>>;

    /// Cancel a bundle by UUID
    async fn cancel_bundle(&self, id: Uuid) -> Result<()>;

    /// Select the candidate bundles to include in the next Flashblock
    async fn select_bundles(&self, filter: BundleFilter) -> Result<Vec<BundleWithMetadata>>;

    /// Find bundle ID by transaction hash
    async fn find_bundle_by_transaction_hash(&self, tx_hash: TxHash) -> Result<Option<Uuid>>;

    /// Remove bundles by IDs
    async fn remove_bundles(&self, ids: Vec<Uuid>) -> Result<usize>;

    /// Update bundle states for multiple bundles
    /// Returns the number of rows that were actually updated
    async fn update_bundles_state(
        &self,
        uuids: Vec<Uuid>,
        allowed_prev_states: Vec<BundleState>,
        new_state: BundleState,
    ) -> Result<Vec<Uuid>>;

    /// Get the current block info (latest block and non finalized blocks)
    async fn get_current_block_info(&self) -> Result<Option<BlockInfo>>;

    /// Commit the latest block info (upsert vec of block nums/hashes should be non finalized)
    async fn commit_block_info(&self, blocks: Vec<BlockInfoUpdate>) -> Result<()>;

    /// Finalize all blocks before the given block number (one-way operation)
    async fn finalize_blocks_before(&self, block_number: u64) -> Result<u64>;

    /// Prune blocks that are finalized before block number X
    async fn prune_finalized_blocks(&self, before_block_number: u64) -> Result<u64>;

    /// Get statistics about bundles and transactions grouped by state
    async fn get_stats(&self) -> Result<BundleStats>;

    /// Remove bundles that have timed out (max_timestamp < current_time)
    /// Returns the UUIDs of bundles that were removed
    async fn remove_timed_out_bundles(&self, current_time: u64) -> Result<Vec<Uuid>>;

    /// Remove old IncludedByBuilder bundles that were last updated before the given timestamp
    /// Returns the UUIDs of bundles that were removed
    async fn remove_old_included_bundles(
        &self,
        cutoff_timestamp: DateTime<Utc>,
    ) -> Result<Vec<Uuid>>;
}
