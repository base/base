//! User Operation Receipt Provider
//!
//! This module provides functionality for looking up and building UserOperation receipts.
//! It handles:
//! - Indexed storage lookups (from the AA indexer ExEx)
//! - Log-based searches for UserOperationEvent
//! - Re-hydrating full event data from minimal indexed references
//! - Filtering receipt logs to isolate a specific UserOperation's logs
//! - Building the full UserOperationReceipt with revert reason extraction

use std::sync::Arc;

use alloy_consensus::{transaction::TxHashRef, Sealable, TxReceipt};
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_eth::Log;
use alloy_sol_types::{sol, SolEvent};
use async_trait::async_trait;
use base_account_abstraction_indexer::{
    IndexedUserOperationRef, UserOperationStorage, ENTRYPOINT_V06, ENTRYPOINT_V07, ENTRYPOINT_V08,
    ENTRYPOINT_V09,
};
use reth::api::BlockBody;
use reth_primitives_traits::Block;
use reth_provider::{BlockNumReader, BlockReader, ReceiptProvider};
use tracing::{debug, warn};

// Define EntryPoint events for parsing logs
sol! {
    /// BeforeExecution event emitted before each UserOperation is executed
    /// This marks the start of logs for a specific UserOperation
    #[derive(Debug)]
    event BeforeExecution();

    /// UserOperationEvent emitted by EntryPoint contracts after execution
    /// This marks the end of logs for a specific UserOperation
    #[derive(Debug)]
    event UserOperationEvent(
        bytes32 indexed userOpHash,
        address indexed sender,
        address indexed paymaster,
        uint256 nonce,
        bool success,
        uint256 actualGasCost,
        uint256 actualGasUsed
    );

    /// UserOperationRevertReason emitted when a UserOperation reverts
    #[derive(Debug)]
    event UserOperationRevertReason(
        bytes32 indexed userOpHash,
        address indexed sender,
        uint256 nonce,
        bytes revertReason
    );
}

/// Error type for receipt provider operations
#[derive(Debug, Clone, thiserror::Error)]
pub enum ReceiptError {
    #[error("Transaction receipt not found")]
    ReceiptNotFound,
    #[error("Block not found")]
    BlockNotFound,
    #[error("Invalid log sequence: {0}")]
    InvalidLogSequence(String),
    #[error("Provider error: {0}")]
    ProviderError(String),
    #[error("User operation not found")]
    UserOperationNotFound,
    #[error("Failed to parse UserOperationEvent: {0}")]
    EventParseError(String),
}

/// Full UserOperation data re-hydrated from receipt
///
/// This struct contains all the data from the UserOperationEvent,
/// re-hydrated from the transaction receipt using the minimal
/// `IndexedUserOperationRef` stored by the indexer.
#[derive(Debug, Clone)]
pub struct IndexedUserOperation {
    /// The hash of the UserOperation
    pub user_op_hash: B256,
    /// The sender (smart account) address
    pub sender: Address,
    /// The paymaster address (zero if self-sponsored)
    pub paymaster: Address,
    /// The nonce used for this operation
    pub nonce: U256,
    /// Whether the operation succeeded
    pub success: bool,
    /// Actual gas cost in wei
    pub actual_gas_cost: U256,
    /// Actual gas used
    pub actual_gas_used: U256,
    /// The EntryPoint contract address
    pub entry_point: Address,
    /// Block number where this was included
    pub block_number: u64,
    /// Block hash where this was included
    pub block_hash: B256,
    /// Transaction hash that included this UserOp
    pub transaction_hash: B256,
    /// Transaction index within the block
    pub transaction_index: u64,
    /// Log index within the transaction
    pub log_index: u64,
}

/// Result of looking up a UserOperation
#[derive(Debug, Clone)]
pub struct UserOperationLookupResult {
    /// The full user operation data (re-hydrated from event)
    pub indexed: IndexedUserOperation,
    /// Filtered logs relevant to this specific UserOperation
    pub logs: Vec<Log>,
    /// Revert reason if the operation failed
    pub reason: Bytes,
}

/// Trait for providing UserOperation receipt data
///
/// This abstraction allows for different implementations:
/// - Real provider using reth's BlockReader and ReceiptProvider
/// - Mock provider for testing
#[async_trait]
pub trait UserOperationReceiptProvider: Send + Sync {
    /// Look up a UserOperation by its hash
    ///
    /// Returns the indexed operation data along with filtered logs
    async fn lookup_user_operation(
        &self,
        user_op_hash: B256,
    ) -> Result<Option<UserOperationLookupResult>, ReceiptError>;

    /// Get the list of supported entry points
    fn supported_entry_points(&self) -> Vec<Address> {
        vec![ENTRYPOINT_V06, ENTRYPOINT_V07, ENTRYPOINT_V08, ENTRYPOINT_V09]
    }
}

/// Filter receipt logs to only include those matching a specific UserOperation.
///
/// This function takes a UserOperation hash and a transaction receipt's logs and filters out
/// all the logs relevant to the user operation. Since there can be multiple user operations
/// in a transaction, we find logs sandwiched between BeforeExecution events.
///
/// The entrypoint will process a whole userOperation prior to moving on, emitting:
/// - BeforeExecution event at the start
/// - Various logs during execution
/// - UserOperationEvent at the end
pub fn filter_logs_for_user_operation(
    entry_point: Address,
    user_op_hash: B256,
    receipt_logs: &[alloy_primitives::Log],
    tx_hash: B256,
    block_hash: Option<B256>,
    block_number: Option<u64>,
) -> Result<Vec<Log>, ReceiptError> {
    let before_execution_topic = BeforeExecution::SIGNATURE_HASH;
    let user_op_event_topic = UserOperationEvent::SIGNATURE_HASH;

    debug!(
        target: "aa-receipt",
        user_op_hash = %user_op_hash,
        entry_point = %entry_point,
        tx_hash = %tx_hash,
        total_logs = receipt_logs.len(),
        "Filtering receipt logs for UserOperation"
    );

    // Helper: Check if log is the reference UserOperationEvent (matching hash)
    let is_ref_user_op = |log: &alloy_primitives::Log| {
        log.topics().len() >= 2
            && log.topics()[0] == user_op_event_topic
            && log.topics()[1] == user_op_hash
            && (log.address == ENTRYPOINT_V06
                || log.address == ENTRYPOINT_V07
                || log.address == ENTRYPOINT_V08)
    };

    // Helper: Check if log is any UserOperationEvent (not necessarily ours)
    let is_user_op_event = |log: &alloy_primitives::Log| {
        !log.topics().is_empty() && log.topics()[0] == user_op_event_topic
    };

    // Helper: Check if log is a BeforeExecution event from the entry point
    let is_before_execution_log = |log: &alloy_primitives::Log| {
        !log.topics().is_empty()
            && log.topics()[0] == before_execution_topic
            && log.address == entry_point
    };

    let mut i = 0;
    let mut start_idx = None;

    while i < receipt_logs.len() {
        let log = &receipt_logs[i];

        if is_before_execution_log(log) || (is_user_op_event(log) && !is_ref_user_op(log)) {
            // Found a boundary - logs for next user op start after this
            start_idx = Some(i + 1);
        } else if is_ref_user_op(log) {
            // Found our UserOperationEvent
            let Some(start) = start_idx else {
                warn!(
                    target: "aa-receipt",
                    user_op_hash = %user_op_hash,
                    log_idx = i,
                    "Found reference user op before BeforeExecution event"
                );
                return Err(ReceiptError::InvalidLogSequence(
                    "Found reference user op before BeforeExecution event".to_string(),
                ));
            };

            debug!(
                target: "aa-receipt",
                user_op_hash = %user_op_hash,
                start_idx = start,
                end_idx = i,
                filtered_logs_count = i - start + 1,
                "Found reference UserOperationEvent, extracting logs"
            );

            // Return logs from start_idx to current index (inclusive)
            return Ok(receipt_logs[start..=i]
                .iter()
                .enumerate()
                .map(|(idx, log)| Log {
                    inner: log.clone(),
                    block_hash,
                    block_number,
                    block_timestamp: None,
                    transaction_hash: Some(tx_hash),
                    transaction_index: None,
                    log_index: Some((start + idx) as u64),
                    removed: false,
                })
                .collect());
        }
        i += 1;
    }

    warn!(
        target: "aa-receipt",
        user_op_hash = %user_op_hash,
        tx_hash = %tx_hash,
        total_logs = receipt_logs.len(),
        "No matching user op found in transaction receipt"
    );
    Err(ReceiptError::InvalidLogSequence(
        "No matching user op found in transaction receipt".to_string(),
    ))
}

/// Extract revert reason from logs for a failed UserOperation
///
/// Looks for UserOperationRevertReason event matching the user_op_hash
pub fn extract_revert_reason(logs: &[Log], user_op_hash: B256) -> Bytes {
    let revert_reason_topic = UserOperationRevertReason::SIGNATURE_HASH;

    for log in logs {
        // Check if this is a UserOperationRevertReason event
        if log.topics().len() < 2 {
            continue;
        }
        if log.topics()[0] != revert_reason_topic {
            continue;
        }
        if log.topics()[1] != user_op_hash {
            continue;
        }

        // Try to decode the revert reason
        if let Ok(event) = UserOperationRevertReason::decode_log(&log.inner) {
            return event.revertReason.clone();
        }
    }

    Bytes::default()
}

/// Default implementation using indexed storage and reth provider
///
/// This provider first checks the indexed storage (populated by the AA indexer ExEx),
/// then falls back to searching recent blocks if not found.
pub struct RethReceiptProvider<Provider> {
    provider: Provider,
    storage: Option<Arc<UserOperationStorage>>,
    lookback_blocks: u64,
}

impl<Provider> RethReceiptProvider<Provider> {
    /// Create a new receipt provider
    pub fn new(
        provider: Provider,
        storage: Option<Arc<UserOperationStorage>>,
        lookback_blocks: u64,
    ) -> Self {
        Self {
            provider,
            storage,
            lookback_blocks,
        }
    }
}

impl<Provider> RethReceiptProvider<Provider>
where
    Provider: BlockNumReader + ReceiptProvider + BlockReader + Send + Sync,
{
    /// Look up a UserOperation from indexed storage and re-hydrate full data
    ///
    /// The indexer stores only minimal refs (tx_hash + log_index).
    /// This method fetches the receipt and parses the event to get full data.
    fn lookup_from_storage(&self, user_op_hash: &B256) -> Result<Option<IndexedUserOperation>, ReceiptError> {
        let Some(op_ref) = self.storage.as_ref().and_then(|s| s.get(user_op_hash)) else {
            return Ok(None);
        };

        // Re-hydrate full data from the receipt
        self.rehydrate_from_ref(&op_ref).map(Some)
    }

    /// Re-hydrate full IndexedUserOperation from a minimal ref
    ///
    /// Fetches the transaction receipt and parses the UserOperationEvent
    /// at the stored log index.
    fn rehydrate_from_ref(
        &self,
        op_ref: &IndexedUserOperationRef,
    ) -> Result<IndexedUserOperation, ReceiptError> {
        // Get the transaction receipt
        let tx_receipt = self
            .provider
            .receipt_by_hash(op_ref.transaction_hash)
            .map_err(|e| {
                ReceiptError::ProviderError(format!("Failed to get transaction receipt: {}", e))
            })?
            .ok_or(ReceiptError::ReceiptNotFound)?;

        // Get the log at the stored index
        let logs = tx_receipt.logs();
        let log = logs.get(op_ref.log_index as usize).ok_or_else(|| {
            ReceiptError::EventParseError(format!(
                "Log index {} out of bounds (receipt has {} logs)",
                op_ref.log_index,
                logs.len()
            ))
        })?;

        // Parse the UserOperationEvent
        let event = UserOperationEvent::decode_log(log).map_err(|e| {
            ReceiptError::EventParseError(format!("Failed to decode UserOperationEvent: {}", e))
        })?;

        // Get block hash and transaction index from the block
        let (block_hash, tx_index) = self
            .provider
            .block(op_ref.block_number.into())
            .ok()
            .flatten()
            .map(|block| {
                let block_hash = block.header().hash_slow();
                let tx_index = block
                    .body()
                    .transactions()
                    .iter()
                    .position(|tx| *tx.tx_hash() == op_ref.transaction_hash)
                    .unwrap_or(0) as u64;
                (block_hash, tx_index)
            })
            .unwrap_or((B256::ZERO, 0));

        Ok(IndexedUserOperation {
            user_op_hash: op_ref.user_op_hash,
            sender: event.sender,
            paymaster: event.paymaster,
            nonce: event.nonce,
            success: event.success,
            actual_gas_cost: event.actualGasCost,
            actual_gas_used: event.actualGasUsed,
            entry_point: log.address,
            block_number: op_ref.block_number,
            block_hash,
            transaction_hash: op_ref.transaction_hash,
            transaction_index: tx_index,
            log_index: op_ref.log_index,
        })
    }

    /// Search for a UserOperationEvent in recent blocks
    ///
    /// This is the fallback when the UserOperation is not found in indexed storage.
    /// It scans recent blocks looking for the UserOperationEvent.
    fn search_user_op_event_in_logs(
        &self,
        user_op_hash: B256,
    ) -> Result<Option<IndexedUserOperation>, ReceiptError> {
        let latest_block = self
            .provider
            .last_block_number()
            .map_err(|e| ReceiptError::ProviderError(format!("Failed to get latest block: {}", e)))?;

        let from_block = latest_block.saturating_sub(self.lookback_blocks);

        debug!(
            target: "aa-receipt",
            user_op_hash = %user_op_hash,
            from_block = from_block,
            to_block = latest_block,
            "Searching for UserOperationEvent in logs"
        );

        // Query logs from all EntryPoint contracts
        let entry_points = vec![ENTRYPOINT_V06, ENTRYPOINT_V07, ENTRYPOINT_V08, ENTRYPOINT_V09];

        for block_num in (from_block..=latest_block).rev() {
            // Get receipts for this block
            let receipts = self
                .provider
                .receipts_by_block(block_num.into())
                .map_err(|e| {
                    ReceiptError::ProviderError(format!(
                        "Failed to get receipts for block {}: {}",
                        block_num, e
                    ))
                })?;

            let Some(receipts) = receipts else {
                continue;
            };

            for (tx_idx, receipt) in receipts.iter().enumerate() {
                for (log_idx, log) in receipt.logs().iter().enumerate() {
                    // Check if this log is from an EntryPoint
                    if !entry_points.contains(&log.address) {
                        continue;
                    }

                    // Try to decode as UserOperationEvent
                    let Ok(event) = UserOperationEvent::decode_log(log) else {
                        continue;
                    };

                    // Check if this is the UserOperation we're looking for
                    if event.userOpHash != user_op_hash {
                        continue;
                    }

                    // Found it! Get the transaction hash from the block
                    let (tx_hash, block_hash) = self
                        .provider
                        .block(block_num.into())
                        .ok()
                        .flatten()
                        .and_then(|block| {
                            let block_hash = block.header().hash_slow();
                            block
                                .body()
                                .transactions()
                                .get(tx_idx)
                                .map(|tx| (*tx.tx_hash(), block_hash))
                        })
                        .unwrap_or((B256::ZERO, B256::ZERO));

                    debug!(
                        target: "aa-receipt",
                        user_op_hash = %user_op_hash,
                        block_number = block_num,
                        tx_hash = %tx_hash,
                        "Found UserOperationEvent in logs"
                    );

                    return Ok(Some(IndexedUserOperation {
                        user_op_hash,
                        sender: event.sender,
                        paymaster: event.paymaster,
                        nonce: event.nonce,
                        success: event.success,
                        actual_gas_cost: event.actualGasCost,
                        actual_gas_used: event.actualGasUsed,
                        entry_point: log.address,
                        block_number: block_num,
                        block_hash,
                        transaction_hash: tx_hash,
                        transaction_index: tx_idx as u64,
                        log_index: log_idx as u64,
                    }));
                }
            }
        }

        Ok(None)
    }

    /// Build a UserOperationLookupResult from indexed data
    fn build_lookup_result(
        &self,
        indexed: IndexedUserOperation,
    ) -> Result<UserOperationLookupResult, ReceiptError> {
        // Get the transaction receipt for logs
        let tx_receipt = self
            .provider
            .receipt_by_hash(indexed.transaction_hash)
            .map_err(|e| {
                ReceiptError::ProviderError(format!("Failed to get transaction receipt: {}", e))
            })?
            .ok_or(ReceiptError::ReceiptNotFound)?;

        // Filter logs to only those relevant to this specific UserOperation
        let logs = filter_logs_for_user_operation(
            indexed.entry_point,
            indexed.user_op_hash,
            tx_receipt.logs(),
            indexed.transaction_hash,
            Some(indexed.block_hash),
            Some(indexed.block_number),
        )?;

        // Extract revert reason if the operation failed
        let reason = if !indexed.success {
            extract_revert_reason(&logs, indexed.user_op_hash)
        } else {
            Bytes::default()
        };

        Ok(UserOperationLookupResult {
            indexed,
            logs,
            reason,
        })
    }
}

#[async_trait]
impl<Provider> UserOperationReceiptProvider for RethReceiptProvider<Provider>
where
    Provider: BlockNumReader + ReceiptProvider + BlockReader + Send + Sync,
{
    async fn lookup_user_operation(
        &self,
        user_op_hash: B256,
    ) -> Result<Option<UserOperationLookupResult>, ReceiptError> {
        // First, try to look up from indexed storage (re-hydrates from minimal ref)
        match self.lookup_from_storage(&user_op_hash)? {
            Some(indexed) => {
                debug!(
                    target: "aa-receipt",
                    user_op_hash = %user_op_hash,
                    "Found UserOperation in indexed storage, re-hydrated from ref"
                );
                return self.build_lookup_result(indexed).map(Some);
            }
            None => {
                // Fall back to searching in logs
                debug!(
                    target: "aa-receipt",
                    user_op_hash = %user_op_hash,
                    "UserOperation not in index, searching logs"
                );
            }
        }

        match self.search_user_op_event_in_logs(user_op_hash)? {
            Some(indexed) => self.build_lookup_result(indexed).map(Some),
            None => {
                debug!(
                    target: "aa-receipt",
                    user_op_hash = %user_op_hash,
                    "UserOperation not found"
                );
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256};

    /// Helper to create a log with specific topics
    fn create_log(address: Address, topics: Vec<B256>, data: Bytes) -> alloy_primitives::Log {
        alloy_primitives::Log::new(address, topics, data).unwrap()
    }

    #[test]
    fn test_filter_logs_single_user_op() {
        let entry_point = ENTRYPOINT_V06;
        let user_op_hash = b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let tx_hash = b256!("2222222222222222222222222222222222222222222222222222222222222222");
        let sender = address!("3333333333333333333333333333333333333333");
        let paymaster = Address::ZERO;

        // Create logs: BeforeExecution, some execution logs, UserOperationEvent
        let logs = vec![
            // BeforeExecution
            create_log(
                entry_point,
                vec![BeforeExecution::SIGNATURE_HASH],
                Bytes::default(),
            ),
            // Some execution log (e.g., from the smart account)
            create_log(
                sender,
                vec![b256!("4444444444444444444444444444444444444444444444444444444444444444")],
                Bytes::default(),
            ),
            // UserOperationEvent
            create_log(
                entry_point,
                vec![
                    UserOperationEvent::SIGNATURE_HASH,
                    user_op_hash,
                    B256::left_padding_from(sender.as_slice()),
                    B256::left_padding_from(paymaster.as_slice()),
                ],
                // Encode: nonce (0), success (true), actualGasCost (1000), actualGasUsed (100)
                Bytes::from(hex::decode(
                    "0000000000000000000000000000000000000000000000000000000000000000\
                     0000000000000000000000000000000000000000000000000000000000000001\
                     00000000000000000000000000000000000000000000000000000000000003e8\
                     0000000000000000000000000000000000000000000000000000000000000064"
                ).unwrap()),
            ),
        ];

        let result = filter_logs_for_user_operation(
            entry_point,
            user_op_hash,
            &logs,
            tx_hash,
            Some(B256::ZERO),
            Some(100),
        );

        assert!(result.is_ok());
        let filtered = result.unwrap();
        // Should include the execution log and UserOperationEvent (not BeforeExecution)
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].address(), sender);
        assert_eq!(filtered[1].address(), entry_point);
    }

    #[test]
    fn test_filter_logs_multiple_user_ops() {
        let entry_point = ENTRYPOINT_V06;
        let user_op_hash_1 = b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let user_op_hash_2 = b256!("2222222222222222222222222222222222222222222222222222222222222222");
        let tx_hash = b256!("3333333333333333333333333333333333333333333333333333333333333333");
        let sender_1 = address!("4444444444444444444444444444444444444444");
        let sender_2 = address!("5555555555555555555555555555555555555555");
        let paymaster = Address::ZERO;

        // Create logs for two user ops in one transaction
        let logs = vec![
            // First UserOp
            create_log(
                entry_point,
                vec![BeforeExecution::SIGNATURE_HASH],
                Bytes::default(),
            ),
            create_log(
                sender_1,
                vec![b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")],
                Bytes::default(),
            ),
            create_log(
                entry_point,
                vec![
                    UserOperationEvent::SIGNATURE_HASH,
                    user_op_hash_1,
                    B256::left_padding_from(sender_1.as_slice()),
                    B256::left_padding_from(paymaster.as_slice()),
                ],
                Bytes::from(hex::decode(
                    "0000000000000000000000000000000000000000000000000000000000000000\
                     0000000000000000000000000000000000000000000000000000000000000001\
                     00000000000000000000000000000000000000000000000000000000000003e8\
                     0000000000000000000000000000000000000000000000000000000000000064"
                ).unwrap()),
            ),
            // Second UserOp (the one we're looking for)
            create_log(
                entry_point,
                vec![BeforeExecution::SIGNATURE_HASH],
                Bytes::default(),
            ),
            create_log(
                sender_2,
                vec![b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")],
                Bytes::default(),
            ),
            create_log(
                sender_2,
                vec![b256!("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")],
                Bytes::default(),
            ),
            create_log(
                entry_point,
                vec![
                    UserOperationEvent::SIGNATURE_HASH,
                    user_op_hash_2,
                    B256::left_padding_from(sender_2.as_slice()),
                    B256::left_padding_from(paymaster.as_slice()),
                ],
                Bytes::from(hex::decode(
                    "0000000000000000000000000000000000000000000000000000000000000001\
                     0000000000000000000000000000000000000000000000000000000000000001\
                     00000000000000000000000000000000000000000000000000000000000007d0\
                     00000000000000000000000000000000000000000000000000000000000000c8"
                ).unwrap()),
            ),
        ];

        // Look for the second user op
        let result = filter_logs_for_user_operation(
            entry_point,
            user_op_hash_2,
            &logs,
            tx_hash,
            Some(B256::ZERO),
            Some(100),
        );

        assert!(result.is_ok());
        let filtered = result.unwrap();
        // Should include only logs from the second user op (2 execution logs + UserOperationEvent)
        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[0].address(), sender_2);
        assert_eq!(filtered[1].address(), sender_2);
        assert_eq!(filtered[2].address(), entry_point);
    }

    #[test]
    fn test_filter_logs_not_found() {
        let entry_point = ENTRYPOINT_V06;
        let user_op_hash = b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let different_hash = b256!("9999999999999999999999999999999999999999999999999999999999999999");
        let tx_hash = b256!("2222222222222222222222222222222222222222222222222222222222222222");
        let sender = address!("3333333333333333333333333333333333333333");
        let paymaster = Address::ZERO;

        let logs = vec![
            create_log(
                entry_point,
                vec![BeforeExecution::SIGNATURE_HASH],
                Bytes::default(),
            ),
            create_log(
                entry_point,
                vec![
                    UserOperationEvent::SIGNATURE_HASH,
                    different_hash, // Different hash!
                    B256::left_padding_from(sender.as_slice()),
                    B256::left_padding_from(paymaster.as_slice()),
                ],
                Bytes::from(hex::decode(
                    "0000000000000000000000000000000000000000000000000000000000000000\
                     0000000000000000000000000000000000000000000000000000000000000001\
                     00000000000000000000000000000000000000000000000000000000000003e8\
                     0000000000000000000000000000000000000000000000000000000000000064"
                ).unwrap()),
            ),
        ];

        let result = filter_logs_for_user_operation(
            entry_point,
            user_op_hash,
            &logs,
            tx_hash,
            Some(B256::ZERO),
            Some(100),
        );

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ReceiptError::InvalidLogSequence(_)));
    }

    #[test]
    fn test_extract_revert_reason_success() {
        let user_op_hash = b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let sender = address!("3333333333333333333333333333333333333333");
        
        // Encode the revert reason data: nonce (0), offset (64), length (4), "test"
        let revert_data = hex::decode(
            "0000000000000000000000000000000000000000000000000000000000000000\
             0000000000000000000000000000000000000000000000000000000000000040\
             0000000000000000000000000000000000000000000000000000000000000004\
             7465737400000000000000000000000000000000000000000000000000000000"
        ).unwrap();

        let logs = vec![
            Log {
                inner: create_log(
                    ENTRYPOINT_V06,
                    vec![
                        UserOperationRevertReason::SIGNATURE_HASH,
                        user_op_hash,
                        B256::left_padding_from(sender.as_slice()),
                    ],
                    Bytes::from(revert_data),
                ),
                block_hash: None,
                block_number: None,
                block_timestamp: None,
                transaction_hash: None,
                transaction_index: None,
                log_index: None,
                removed: false,
            },
        ];

        let reason = extract_revert_reason(&logs, user_op_hash);
        assert!(!reason.is_empty());
        // The reason should be "test" (0x74657374)
        assert_eq!(&reason[..], b"test");
    }

    #[test]
    fn test_extract_revert_reason_not_found() {
        let user_op_hash = b256!("1111111111111111111111111111111111111111111111111111111111111111");
        
        // Empty logs
        let logs: Vec<Log> = vec![];
        let reason = extract_revert_reason(&logs, user_op_hash);
        assert!(reason.is_empty());
    }

    #[test]
    fn test_extract_revert_reason_different_hash() {
        let user_op_hash = b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let different_hash = b256!("2222222222222222222222222222222222222222222222222222222222222222");
        let sender = address!("3333333333333333333333333333333333333333");
        
        let revert_data = hex::decode(
            "0000000000000000000000000000000000000000000000000000000000000000\
             0000000000000000000000000000000000000000000000000000000000000040\
             0000000000000000000000000000000000000000000000000000000000000004\
             7465737400000000000000000000000000000000000000000000000000000000"
        ).unwrap();

        let logs = vec![
            Log {
                inner: create_log(
                    ENTRYPOINT_V06,
                    vec![
                        UserOperationRevertReason::SIGNATURE_HASH,
                        different_hash, // Different hash!
                        B256::left_padding_from(sender.as_slice()),
                    ],
                    Bytes::from(revert_data),
                ),
                block_hash: None,
                block_number: None,
                block_timestamp: None,
                transaction_hash: None,
                transaction_index: None,
                log_index: None,
                removed: false,
            },
        ];

        let reason = extract_revert_reason(&logs, user_op_hash);
        assert!(reason.is_empty());
    }
}

