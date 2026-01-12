//! Account Abstraction Indexer ExEx
//!
//! An Execution Extension (ExEx) that indexes UserOperation events from 
//! EIP-4337 EntryPoint contracts.

use std::sync::Arc;

use alloy_consensus::TxReceipt;
use eyre::Result;
use futures::StreamExt;
use reth::api::{BlockBody, FullNodeComponents};
use reth::core::primitives::{transaction::TxHashRef, AlloyBlockHeader};
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_tracing::tracing::{debug, info};

use crate::event::try_parse_user_operation_ref;
use crate::storage::UserOperationStorage;

/// Starts the Account Abstraction indexer ExEx
///
/// This ExEx listens for chain notifications and indexes UserOperationEvent logs
/// from EntryPoint contracts (v0.6, v0.7, v0.8).
///
/// # Arguments
/// * `ctx` - The ExEx context providing chain notifications
/// * `storage` - Shared storage for indexed UserOperations
pub async fn account_abstraction_indexer_exex<Node: FullNodeComponents>(
    mut ctx: ExExContext<Node>,
    storage: Arc<UserOperationStorage>,
) -> Result<()> {
    info!(target: "aa-indexer", "Starting Account Abstraction Indexer ExEx");

    loop {
        tokio::select! {
            Some(notification) = ctx.notifications.next() => {
                match notification {
                    Ok(ExExNotification::ChainCommitted { new }) => {
                        let tip = new.tip();
                        let tip_number = tip.header().number();
                        let tip_hash = tip.hash();
                        
                        debug!(
                            target: "aa-indexer",
                            block_number = tip_number,
                            block_hash = %tip_hash,
                            "Processing committed chain"
                        );

                        // Get execution outcome which contains receipts
                        let execution_outcome = new.execution_outcome();
                        
                        // Process all blocks in the committed chain
                        for block in new.blocks().values() {
                            let block_number = block.header().number();
                            let _block_hash = block.hash();
                            
                            // Get receipts for this block from execution outcome
                            let receipts = execution_outcome.receipts_by_block(block_number);
                            
                            // Iterate through transactions and their receipts
                            let transactions = block.body().transactions();
                            
                            for (_tx_idx, (tx, receipt)) in transactions.iter().zip(receipts.iter()).enumerate() {
                                let tx_hash = *tx.tx_hash();
                                
                                // Process each log in the receipt
                                for (log_idx, log) in receipt.logs().iter().enumerate() {
                                    if let Some(op_ref) = try_parse_user_operation_ref(
                                        log,
                                        block_number,
                                        tx_hash,
                                        log_idx as u64,
                                    ) {
                                        debug!(
                                            target: "aa-indexer",
                                            user_op_hash = %op_ref.user_op_hash,
                                            block_number = block_number,
                                            tx_hash = %tx_hash,
                                            log_index = log_idx,
                                            "Indexed UserOperation"
                                        );
                                        storage.insert(op_ref);
                                    }
                                }
                            }
                        }

                        // Notify that we've processed up to this height
                        ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
                        
                        metrics::gauge!("aa_indexer_last_indexed_block").set(tip_number as f64);
                    }
                    
                    Ok(ExExNotification::ChainReorged { old, new }) => {
                        let old_tip = old.tip().header().number();
                        let new_tip = new.tip().header().number();
                        
                        info!(
                            target: "aa-indexer",
                            old_tip = old_tip,
                            new_tip = new_tip,
                            "Chain reorg detected, reindexing"
                        );

                        // Remove operations from reverted blocks
                        // Find the common ancestor (first block in new chain)
                        let reorg_start = new.blocks().keys().min().copied().unwrap_or(0);
                        let removed = storage.remove_from_block(reorg_start);
                        
                        debug!(
                            target: "aa-indexer",
                            removed_count = removed,
                            from_block = reorg_start,
                            "Removed UserOperations due to reorg"
                        );

                        metrics::counter!("aa_indexer_reorg_removals").increment(removed as u64);

                        // Get execution outcome for re-indexing
                        let execution_outcome = new.execution_outcome();

                        // Re-index the new chain
                        for block in new.blocks().values() {
                            let block_number = block.header().number();
                            let _block_hash = block.hash();

                            let receipts = execution_outcome.receipts_by_block(block_number);
                            let transactions = block.body().transactions();

                            for (_tx_idx, (tx, receipt)) in transactions.iter().zip(receipts.iter()).enumerate() {
                                let tx_hash = *tx.tx_hash();

                                for (log_idx, log) in receipt.logs().iter().enumerate() {
                                    if let Some(op_ref) = try_parse_user_operation_ref(
                                        log,
                                        block_number,
                                        tx_hash,
                                        log_idx as u64,
                                    ) {
                                        storage.insert(op_ref);
                                    }
                                }
                            }
                        }

                        ctx.events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
                        metrics::gauge!("aa_indexer_last_indexed_block").set(new_tip as f64);
                    }
                    
                    Ok(ExExNotification::ChainReverted { old }) => {
                        let old_tip = old.tip().header().number();
                        
                        info!(
                            target: "aa-indexer",
                            old_tip = old_tip,
                            "Chain reverted"
                        );

                        // Remove all operations from reverted blocks
                        let revert_start = old.blocks().keys().min().copied().unwrap_or(0);
                        let removed = storage.remove_from_block(revert_start);
                        
                        debug!(
                            target: "aa-indexer",
                            removed_count = removed,
                            from_block = revert_start,
                            "Removed UserOperations due to revert"
                        );

                        metrics::counter!("aa_indexer_revert_removals").increment(removed as u64);

                        ctx.events.send(ExExEvent::FinishedHeight(old.tip().num_hash()))?;
                    }
                    
                    Err(e) => {
                        debug!(target: "aa-indexer", error = %e, "Notification error");
                        return Err(e.into());
                    }
                }
            }
        }
    }
}
