//! Payload attributes builder for stateless execution.
//!
//! This module provides functionality to build payload attributes by extracting
//! deposit transactions from L1 receipts.

use alloy_consensus::{Header, ReceiptEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{B256, Bytes, Log};
use base_consensus_genesis::{L1ChainConfig, RollupConfig, SystemConfig};
use base_protocol::{L1BlockInfoTx, decode_deposit};
use hex_literal::hex;

use crate::error::ExecutorError;

/// Deposit event topic (`TransactionDeposited` event).
/// keccak256("TransactionDeposited(address,address,uint256,bytes)")
pub const DEPOSIT_EVENT_TOPIC: B256 =
    B256::new(hex!("b3813568d9991fc951961fcb4c784893574240a28925604d09fc577c55bb7c32"));

/// Extract deposit transactions from L1 receipts.
///
/// This builds the complete deposit transaction list for an L2 block:
/// 1. First transaction: L1 info deposit tx (records L1 block info on L2)
/// 2. Remaining transactions: User deposits from `TransactionDeposited` events
///
/// # Arguments
///
/// * `rollup_config` - The rollup configuration
/// * `l1_config` - The L1 chain configuration
/// * `system_config` - The current system configuration
/// * `l1_origin` - The L1 origin block header
/// * `l1_origin_hash` - The L1 origin block hash
/// * `receipts` - The L1 origin block receipts
/// * `l2_block_number` - The L2 block number being built
/// * `l2_timestamp` - The L2 block timestamp
/// * `sequence_number` - The sequence number (0 if new L1 origin, else `parent.seq_num` + 1)
#[allow(clippy::too_many_arguments)]
pub fn extract_deposits_from_receipts(
    rollup_config: &RollupConfig,
    l1_config: &L1ChainConfig,
    system_config: &SystemConfig,
    l1_origin: &Header,
    l1_origin_hash: B256,
    receipts: &[ReceiptEnvelope],
    l2_block_number: u64,
    l2_timestamp: u64,
    sequence_number: u64,
) -> Result<Vec<Bytes>, ExecutorError> {
    let mut deposits = Vec::new();

    // 1. Build L1 info deposit transaction (always first)
    let l1_info_deposit = build_l1_info_deposit_tx(
        rollup_config,
        l1_config,
        system_config,
        l1_origin,
        l2_block_number,
        l2_timestamp,
        sequence_number,
    )?;
    deposits.push(l1_info_deposit);

    // 2. Extract user deposits from L1 receipts
    let deposit_contract_address = rollup_config.deposit_contract_address;
    let mut log_index: usize = 0;

    for receipt in receipts {
        // Get logs from the receipt
        let logs = get_receipt_logs(receipt);

        for log in logs {
            // Check if this is a deposit event from the deposit contract
            if log.address == deposit_contract_address
                && !log.topics().is_empty()
                && log.topics()[0] == DEPOSIT_EVENT_TOPIC
            {
                // Parse the deposit transaction using base-protocol
                let deposit_tx = decode_deposit(l1_origin_hash, log_index, log).map_err(|e| {
                    ExecutorError::AttributesBuildFailed(format!("failed to decode deposit: {e}"))
                })?;
                deposits.push(deposit_tx);
            }
            log_index += 1;
        }
    }

    Ok(deposits)
}

/// Get logs from a receipt envelope.
fn get_receipt_logs(receipt: &ReceiptEnvelope) -> &[Log] {
    match receipt {
        ReceiptEnvelope::Legacy(r)
        | ReceiptEnvelope::Eip2930(r)
        | ReceiptEnvelope::Eip1559(r)
        | ReceiptEnvelope::Eip4844(r)
        | ReceiptEnvelope::Eip7702(r) => &r.receipt.logs,
    }
}

/// Build the L1 info deposit transaction.
///
/// This is the first transaction in every L2 block that records L1 block info.
/// Uses the appropriate format based on the active hardfork (Bedrock/Ecotone/Isthmus/Jovian).
fn build_l1_info_deposit_tx(
    rollup_config: &RollupConfig,
    l1_config: &L1ChainConfig,
    system_config: &SystemConfig,
    l1_origin: &Header,
    _l2_block_number: u64,
    l2_timestamp: u64,
    sequence_number: u64,
) -> Result<Bytes, ExecutorError> {
    // Use base-protocol's L1BlockInfoTx to build the deposit transaction
    let (_l1_info, deposit_tx) = L1BlockInfoTx::try_new_with_deposit_tx(
        rollup_config,
        l1_config,
        system_config,
        sequence_number,
        l1_origin,
        l2_timestamp,
    )
    .map_err(|e| {
        ExecutorError::AttributesBuildFailed(format!("failed to build L1 info deposit tx: {e}"))
    })?;

    // Encode the deposit transaction
    let mut encoded = Vec::new();
    deposit_tx.encode_2718(&mut encoded);

    Ok(Bytes::from(encoded))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use base_protocol::{Predeploys, SystemAddresses};

    use super::*;

    #[test]
    fn test_deposit_event_topic() {
        // Verify the deposit event topic is correct
        // keccak256("TransactionDeposited(address,address,uint256,bytes)")
        let expected = B256::new(hex_literal::hex!(
            "b3813568d9991fc951961fcb4c784893574240a28925604d09fc577c55bb7c32"
        ));
        assert_eq!(DEPOSIT_EVENT_TOPIC, expected);
    }

    #[test]
    fn test_l1_attributes_addresses() {
        // Verify the predefined addresses are correct
        assert_eq!(
            SystemAddresses::DEPOSITOR_ACCOUNT,
            address!("deaddeaddeaddeaddeaddeaddeaddeaddead0001")
        );
        assert_eq!(Predeploys::L1_BLOCK_INFO, address!("4200000000000000000000000000000000000015"));
    }
}
