//! Block Watcher
//!
//! Watches for new blocks and removes included UserOperations from the mempool.
//! Also updates reputation for entities whose UserOps were included.
//!
//! # Architecture
//!
//! The block watcher receives ExEx notifications and processes them:
//! - `ChainCommitted`: Remove included UserOps, update reputation
//! - `ChainReorged`: Remove UserOps from new chain (reverted ops are NOT re-added)
//! - `ChainReverted`: No action (we don't have validation data to re-add)
//!
//! # Note on Reorgs
//!
//! When a reorg occurs, UserOps that were included in the old chain but not in the
//! new chain are NOT automatically re-added to the mempool. This is because:
//! 1. We don't store the original validation output needed for mempool inclusion
//! 2. The validation might be invalidated by the reorg anyway
//! 3. The sender should re-submit if they want the UserOp included
//!
//! # Integration with Builder
//!
//! The builder can mark UserOps as "pending inclusion" before building a bundle.
//! If the bundle is included, the block watcher will remove them. If not, the
//! builder should call `release_pending` to make them available again.

use std::sync::Arc;

use alloy_consensus::TxReceipt;
use alloy_primitives::{Address, B256, Log};
use alloy_sol_types::SolEvent;
use base_account_abstraction_indexer::UserOperationEvent;
use parking_lot::RwLock;
use reth::api::{BlockBody, NodePrimitives};
use reth::core::primitives::{transaction::TxHashRef, AlloyBlockHeader};
use reth::providers::Chain;
use reth_exex::{ExExEvent, ExExNotification};
use tracing::{debug, info};

use crate::contracts::is_entrypoint;

use super::pool::UserOpPool;

/// Parsed UserOperationEvent - minimal data needed for mempool updates
#[derive(Debug, Clone)]
pub struct ParsedUserOpEvent {
    /// The UserOperation hash
    pub user_op_hash: B256,
    /// The EntryPoint that emitted this event
    pub entry_point: Address,
    /// Block number where this was included
    pub block_number: u64,
}

/// Try to parse a UserOperationEvent from a log
///
/// Only extracts the userOpHash from the indexed topics - we don't need the
/// full event data (sender, paymaster, success, etc.) for mempool updates.
fn try_parse_user_op_event(log: &Log, block_number: u64) -> Option<ParsedUserOpEvent> {
    // Check if this log is from a known EntryPoint
    if !is_entrypoint(log.address) {
        return None;
    }

    // Check if this is a UserOperationEvent (topic[0] == event signature)
    // and has at least 2 topics (signature + userOpHash)
    let topics = log.topics();
    if topics.len() < 2 || topics[0] != UserOperationEvent::SIGNATURE_HASH {
        return None;
    }

    // topic[1] is the indexed userOpHash
    let user_op_hash = topics[1];

    Some(ParsedUserOpEvent {
        user_op_hash,
        entry_point: log.address,
        block_number,
    })
}

/// Watches for blocks and updates the mempool
pub struct BlockWatcher {
    /// Reference to the mempool
    pool: Arc<RwLock<UserOpPool>>,
}

impl BlockWatcher {
    /// Create a new block watcher
    pub fn new(pool: Arc<RwLock<UserOpPool>>) -> Self {
        Self { pool }
    }

    /// Handle an ExEx notification
    ///
    /// Processes chain events and updates the mempool accordingly.
    /// Returns an ExExEvent to acknowledge processing.
    pub fn handle_notification<N: NodePrimitives>(
        &self,
        notification: ExExNotification<N>,
    ) -> ExExEvent {
        match notification {
            ExExNotification::ChainCommitted { new } => {
                self.process_committed_chain(&new);
                ExExEvent::FinishedHeight(new.tip().num_hash())
            }
            ExExNotification::ChainReorged { old, new } => {
                debug!(
                    target: "aa-mempool",
                    old_tip = ?old.tip().number(),
                    new_tip = ?new.tip().number(),
                    "Chain reorg detected"
                );
                // Process the new chain (remove UserOps that are now included)
                // Note: We do NOT re-add UserOps from the old chain
                self.process_committed_chain(&new);
                ExExEvent::FinishedHeight(new.tip().num_hash())
            }
            ExExNotification::ChainReverted { old } => {
                debug!(
                    target: "aa-mempool",
                    old_tip = ?old.tip().number(),
                    "Chain reverted"
                );
                // We don't re-add UserOps on revert - they would need re-validation
                ExExEvent::FinishedHeight(old.tip().num_hash())
            }
        }
    }

    /// Process a committed chain and remove included UserOps
    fn process_committed_chain<N: NodePrimitives>(&self, chain: &Chain<N>) {
        let mut total_removed = 0;

        // Get execution outcome for receipts
        let execution_outcome = chain.execution_outcome();

        for block in chain.blocks().values() {
            let block_number = block.header().number();

            // Get receipts for this block
            let receipts = execution_outcome.receipts_by_block(block_number);
            let transactions = block.body().transactions();

            // Process each transaction and its receipt
            for (tx, receipt) in transactions.iter().zip(receipts.iter()) {
                let tx_hash = *tx.tx_hash();

                // Process each log in the receipt
                for log in receipt.logs() {
                    if let Some(event) = try_parse_user_op_event(log, block_number) {
                        if self.handle_user_op_event(&event, tx_hash) {
                            total_removed += 1;
                        }
                    }
                }
            }
        }

        if total_removed > 0 {
            info!(
                target: "aa-mempool",
                removed = total_removed,
                blocks = chain.blocks().len(),
                "Processed committed chain"
            );
        }
    }

    /// Handle a single UserOperationEvent
    ///
    /// Returns true if the UserOp was found and removed from the mempool.
    fn handle_user_op_event(&self, event: &ParsedUserOpEvent, tx_hash: B256) -> bool {
        let mut pool = self.pool.write();

        // Try to remove the UserOp from the mempool
        let removed = pool.remove(&event.entry_point, &event.user_op_hash);

        if let Some(op) = removed {
            debug!(
                target: "aa-mempool",
                hash = %event.user_op_hash,
                sender = %op.sender,
                entry_point = %event.entry_point,
                tx_hash = %tx_hash,
                "UserOp included in block"
            );

            // Update reputation - UserOp landed on-chain means bundler got paid
            // We don't care about execution success/failure for reputation
            pool.reputation.record_included(
                op.sender,
                op.paymaster,
                op.factory,
                op.aggregator,
            );

            true
        } else {
            // UserOp wasn't in our mempool - that's okay, it might have been:
            // 1. Already removed by the builder via confirm_included
            // 2. From a different bundler
            // 3. Directly submitted to the EntryPoint
            debug!(
                target: "aa-mempool",
                hash = %event.user_op_hash,
                "UserOp not found in mempool (already removed or from different source)"
            );
            false
        }
    }

    /// Manually process a batch of included UserOps
    ///
    /// This is useful for testing or when the block watcher isn't running.
    pub fn process_included(&self, entry_point: Address, user_op_hashes: &[B256]) {
        let mut pool = self.pool.write();
        let removed = pool.confirm_included(&entry_point, user_op_hashes);

        if !removed.is_empty() {
            debug!(
                target: "aa-mempool",
                entry_point = %entry_point,
                count = removed.len(),
                "Manually processed included UserOps"
            );
        }
    }

    /// Get the current mempool size
    pub fn mempool_size(&self) -> usize {
        self.pool.read().total_count()
    }

    /// Get mempool size for a specific entrypoint
    pub fn mempool_size_for_entrypoint(&self, entry_point: &Address) -> usize {
        self.pool.read().count(entry_point)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{
        ENTRYPOINT_V06_ADDRESS, ENTRYPOINT_V07_ADDRESS, ENTRYPOINT_V08_ADDRESS,
        ENTRYPOINT_V09_ADDRESS,
    };
    use crate::mempool::config::MempoolConfig;

    #[test]
    fn test_event_signature() {
        // Verify the event signature from alloy matches EIP-4337 spec
        assert_eq!(
            UserOperationEvent::SIGNATURE,
            "UserOperationEvent(bytes32,address,address,uint256,bool,uint256,uint256)"
        );
    }

    #[test]
    fn test_is_entry_point() {
        assert!(is_entrypoint(ENTRYPOINT_V06_ADDRESS));
        assert!(is_entrypoint(ENTRYPOINT_V07_ADDRESS));
        assert!(is_entrypoint(ENTRYPOINT_V08_ADDRESS));
        assert!(is_entrypoint(ENTRYPOINT_V09_ADDRESS));
        assert!(!is_entrypoint(Address::ZERO));
    }

    #[test]
    fn test_block_watcher_creation() {
        let config = MempoolConfig::default();
        let pool = UserOpPool::new_shared(config, 1);
        let _watcher = BlockWatcher::new(pool);
    }

    #[test]
    fn test_process_included() {
        let config = MempoolConfig::default();
        let pool = UserOpPool::new_shared(config, 1);
        let watcher = BlockWatcher::new(pool.clone());

        // Process non-existent UserOps (should be a no-op)
        let hashes = vec![B256::ZERO, B256::from([1u8; 32])];
        watcher.process_included(ENTRYPOINT_V07_ADDRESS, &hashes);

        assert_eq!(watcher.mempool_size(), 0);
    }
}
