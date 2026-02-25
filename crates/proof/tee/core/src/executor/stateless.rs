//! Stateless block execution.
//!
//! This module provides the core stateless block execution functionality,
//! porting the Go implementation from `stateless.go`.

use alloy_consensus::{Header, ReceiptEnvelope, Sealable};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B256, Bytes, address};
use base_alloy_consensus::OpTxEnvelope;
use kona_genesis::{L1ChainConfig, RollupConfig};

use super::{
    attributes::extract_deposits_from_receipts,
    evm::{build_l1_block_info_from_deposit, execute_block},
    l2_block_ref::l2_block_to_block_info,
    trie_db::EnclaveTrieDB,
    witness::{ExecutionWitness, transform_witness},
};
use crate::{
    error::ExecutorError,
    providers::{L2SystemConfigFetcher, compute_l1_receipt_root, compute_tx_root},
    types::account::AccountResult,
};

/// Maximum sequencer drift in seconds (Fjord hardfork).
/// If a block's timestamp exceeds `l1_origin.timestamp` + `MAX_SEQUENCER_DRIFT_FJORD`,
/// the block can only contain deposit transactions.
pub const MAX_SEQUENCER_DRIFT_FJORD: u64 = 1800;

/// L2 to L1 Message Passer predeploy address.
pub const L2_TO_L1_MESSAGE_PASSER: Address = address!("4200000000000000000000000000000000000016");

/// Result of stateless execution.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// The computed state root after execution.
    pub state_root: B256,

    /// The computed receipt hash after execution.
    pub receipt_hash: B256,
}

/// Execute stateless block validation with full EVM execution.
///
/// This validates a block without maintaining full state by using a witness
/// that provides the necessary state data. It performs all validation checks
/// from the Go implementation in `stateless.go` and executes the block via
/// kona-executor's `StatelessL2Builder`.
///
/// # Validation Checks
///
/// 1. L1 receipts hash matches the L1 origin header
/// 2. Block parent hash matches the previous block header
/// 3. Sequencer drift check (block timestamp vs L1 origin timestamp)
/// 4. Previous block transactions hash matches header
/// 5. L1 origin is valid for the L2 parent block
/// 6. No deposit transactions in sequenced transactions
/// 7. State root matches after execution
/// 8. Receipt hash matches after execution
/// 9. Message account proof is valid
///
/// # Arguments
///
/// * `rollup_config` - The rollup configuration
/// * `l1_config` - The L1 chain configuration (for deposit extraction)
/// * `l1_origin` - The L1 origin block header
/// * `l1_receipts` - The L1 origin block receipts
/// * `previous_block_txs` - Transactions from the previous L2 block (RLP-encoded)
/// * `block_header` - The L2 block header to validate
/// * `sequenced_txs` - Sequenced transactions for this block (RLP-encoded)
/// * `witness` - The execution witness
/// * `message_account` - The `L2ToL1MessagePasser` account proof
///
/// # Returns
///
/// The execution result containing computed roots.
///
/// # Errors
///
/// Returns an error if any validation check fails or EVM execution fails.
#[allow(clippy::too_many_arguments)]
pub fn execute_stateless(
    rollup_config: &RollupConfig,
    l1_config: &L1ChainConfig,
    l1_origin: &Header,
    l1_receipts: &[ReceiptEnvelope],
    previous_block_txs: &[Bytes],
    block_header: &Header,
    sequenced_txs: &[Bytes],
    witness: ExecutionWitness,
    message_account: &AccountResult,
) -> Result<ExecutionResult, ExecutorError> {
    // 1. Verify L1 receipts hash (stateless.go:34-37)
    let computed_receipt_root = compute_l1_receipt_root(l1_receipts);
    if computed_receipt_root != l1_origin.receipts_root {
        return Err(ExecutorError::InvalidReceipts);
    }

    // Transform the witness
    let transformed = transform_witness(witness)?;

    // 2. Verify parent hash (stateless.go:39-43)
    // Clone the header upfront since we'll consume transformed later
    let previous_header = transformed.previous_header().clone();
    let previous_block_hash = previous_header.hash_slow();
    if block_header.parent_hash != previous_block_hash {
        return Err(ExecutorError::InvalidParentHash);
    }

    // 3. Check sequencer drift (stateless.go:46-48)
    // Block must only contain deposit transactions if it is outside the sequencer drift
    if !sequenced_txs.is_empty()
        && block_header.timestamp > l1_origin.timestamp + MAX_SEQUENCER_DRIFT_FJORD
    {
        return Err(ExecutorError::L1OriginTooOld);
    }

    // 4. Verify previous block transactions hash (stateless.go:60-68)
    let previous_txs_rlp: Vec<Vec<u8>> = previous_block_txs.iter().map(|tx| tx.to_vec()).collect();
    let previous_tx_hash = compute_tx_root(&previous_txs_rlp);
    if previous_tx_hash != previous_header.transactions_root {
        return Err(ExecutorError::InvalidTxHash);
    }

    // 5. Verify L1 origin is valid (stateless.go:74-81)
    // Parse L2BlockInfo from the previous block to get its L1 origin
    let first_prev_tx = previous_block_txs.first().ok_or_else(|| {
        ExecutorError::ExecutionFailed("previous block has no transactions".to_string())
    })?;

    let l2_parent = l2_block_to_block_info(
        rollup_config,
        &previous_header,
        previous_block_hash,
        first_prev_tx,
    )?;

    let l1_origin_hash = l1_origin.hash_slow();

    // The L2 parent's L1 origin must be either the current L1 origin or its parent
    if l2_parent.l1_origin.hash != l1_origin_hash
        && l2_parent.l1_origin.hash != l1_origin.parent_hash
    {
        return Err(ExecutorError::InvalidL1Origin);
    }

    // Check if L1 origin changed (need deposits from this L1 block)
    let include_deposits = l2_parent.l1_origin.hash != l1_origin_hash;

    // Calculate sequence number for deposit building
    let sequence_number = if include_deposits {
        0 // New L1 origin, start sequence at 0
    } else {
        l2_parent.seq_num + 1 // Same L1 origin, increment sequence
    };

    // 6. Check sequenced transactions don't include deposits (stateless.go:100-104)
    for tx_bytes in sequenced_txs {
        let tx = OpTxEnvelope::decode_2718(&mut tx_bytes.as_ref())
            .map_err(|e| ExecutorError::TxDecodeFailed(e.to_string()))?;

        if tx.is_deposit() {
            return Err(ExecutorError::SequencedTxCannotBeDeposit);
        }
    }

    // Create the TrieDB from the transformed witness
    let trie_db = EnclaveTrieDB::from_witness(transformed);

    // 7-8. Build all transactions and execute via EVM
    // Get system config from L2 fetcher using previous block
    // Extract the calldata from the first deposit transaction (used for both L2SystemConfigFetcher and L1BlockInfo)
    let first_prev_deposit_calldata = {
        let first_prev_tx = previous_block_txs.first().ok_or_else(|| {
            ExecutorError::ExecutionFailed("previous block has no transactions".to_string())
        })?;
        let tx = OpTxEnvelope::decode_2718(&mut first_prev_tx.as_ref())
            .map_err(|e| ExecutorError::TxDecodeFailed(e.to_string()))?;
        match tx {
            OpTxEnvelope::Deposit(d) => d.input.clone(),
            _ => {
                return Err(ExecutorError::ExecutionFailed(
                    "first previous block transaction is not a deposit".to_string(),
                ));
            }
        }
    };
    let l2_fetcher = L2SystemConfigFetcher::new(
        rollup_config.clone(),
        previous_block_hash,
        previous_header.clone(),
        Some(first_prev_deposit_calldata.clone()),
    );
    let system_config = l2_fetcher.system_config_by_l2_hash(previous_block_hash)?;

    // Extract deposit transactions from L1 receipts
    // If include_deposits is false (same L1 origin), pass empty receipts to skip user deposits
    // but still generate the L1 info deposit tx
    let receipts_for_deposits: &[ReceiptEnvelope] =
        if include_deposits { l1_receipts } else { &[] };
    let deposits = extract_deposits_from_receipts(
        rollup_config,
        l1_config,
        &system_config,
        l1_origin,
        l1_origin_hash,
        receipts_for_deposits,
        block_header.number,
        block_header.timestamp,
        sequence_number,
    )?;

    // Combine deposits + sequenced transactions
    let all_txs = [deposits, sequenced_txs.to_vec()].concat();

    // Create sealed parent header for builder
    let parent_sealed = previous_header.seal_slow();

    // Build L1BlockInfo from the PREVIOUS block's L1 info deposit
    // This represents what was in the L1Block contract at the START of this block's execution.
    // The first transaction (L1 info deposit) will update these values, but for the EVM context
    // during execution, we pre-populate with the previous values so that L1 fee calculations
    // work correctly for transactions before the L1Block contract is read.
    let spec_id = rollup_config.spec_id(block_header.timestamp);
    let l1_block_info = build_l1_block_info_from_deposit(&first_prev_deposit_calldata, spec_id)
        .map_err(|e| ExecutorError::ExecutionFailed(format!("Failed to build L1BlockInfo: {e}")))?;

    // Execute block via StatelessL2Builder
    let execution_result = execute_block(
        rollup_config,
        parent_sealed,
        block_header,
        &all_txs,
        trie_db,
        l1_block_info,
    )?;

    // Verify state root matches
    if execution_result.state_root != block_header.state_root {
        return Err(ExecutorError::InvalidStateRoot {
            expected: block_header.state_root,
            computed: execution_result.state_root,
        });
    }

    // Verify receipts root matches
    if execution_result.receipts_root != block_header.receipts_root {
        return Err(ExecutorError::InvalidReceiptHash {
            expected: block_header.receipts_root,
            computed: execution_result.receipts_root,
        });
    }

    // 9. Verify message account (stateless.go:132-137)
    if message_account.address != L2_TO_L1_MESSAGE_PASSER {
        return Err(ExecutorError::InvalidMessageAccountAddress);
    }
    message_account
        .verify(execution_result.state_root)
        .map_err(|e| ExecutorError::MessageAccountVerificationFailed(e.to_string()))?;

    Ok(ExecutionResult {
        state_root: execution_result.state_root,
        receipt_hash: execution_result.receipts_root,
    })
}

/// Validates that a transaction is not a deposit transaction.
///
/// # Arguments
///
/// * `tx_bytes` - The RLP-encoded transaction bytes
///
/// # Returns
///
/// `true` if the transaction is NOT a deposit, `false` if it is a deposit.
///
/// # Errors
///
/// Returns an error if the transaction cannot be decoded.
pub fn validate_not_deposit(tx_bytes: &Bytes) -> Result<bool, ExecutorError> {
    let tx = OpTxEnvelope::decode_2718(&mut tx_bytes.as_ref())
        .map_err(|e| ExecutorError::TxDecodeFailed(e.to_string()))?;

    Ok(!tx.is_deposit())
}

/// Validates the sequencer drift constraint.
///
/// If there are sequenced transactions, the block timestamp must be within
/// `MAX_SEQUENCER_DRIFT_FJORD` seconds of the L1 origin timestamp.
///
/// # Arguments
///
/// * `block_timestamp` - The L2 block timestamp
/// * `l1_origin_timestamp` - The L1 origin block timestamp
/// * `has_sequenced_txs` - Whether the block has sequenced transactions
///
/// # Returns
///
/// `true` if the constraint is satisfied, `false` otherwise.
#[must_use]
pub const fn validate_sequencer_drift(
    block_timestamp: u64,
    l1_origin_timestamp: u64,
    has_sequenced_txs: bool,
) -> bool {
    if !has_sequenced_txs {
        return true;
    }
    block_timestamp <= l1_origin_timestamp + MAX_SEQUENCER_DRIFT_FJORD
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_sequencer_drift_no_txs() {
        // No sequenced txs, should always pass
        assert!(validate_sequencer_drift(10000, 1000, false));
    }

    #[test]
    fn test_validate_sequencer_drift_within_limit() {
        // Within MAX_SEQUENCER_DRIFT_FJORD (1800 seconds)
        assert!(validate_sequencer_drift(2800, 1000, true));
    }

    #[test]
    fn test_validate_sequencer_drift_at_limit() {
        // Exactly at MAX_SEQUENCER_DRIFT_FJORD
        assert!(validate_sequencer_drift(1000 + MAX_SEQUENCER_DRIFT_FJORD, 1000, true));
    }

    #[test]
    fn test_validate_sequencer_drift_exceeds_limit() {
        // Exceeds MAX_SEQUENCER_DRIFT_FJORD
        assert!(!validate_sequencer_drift(1000 + MAX_SEQUENCER_DRIFT_FJORD + 1, 1000, true));
    }

    #[test]
    fn test_l2_to_l1_message_passer_address() {
        assert_eq!(L2_TO_L1_MESSAGE_PASSER, address!("4200000000000000000000000000000000000016"));
    }

    #[test]
    fn test_max_sequencer_drift() {
        assert_eq!(MAX_SEQUENCER_DRIFT_FJORD, 1800);
    }
}
