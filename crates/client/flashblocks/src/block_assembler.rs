//! Block assembly from flashblocks.
//!
//! This module provides the [`BlockAssembler`] which reconstructs blocks from flashblocks.

use alloy_consensus::{Header, Sealed};
use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types::Withdrawal;
use alloy_rpc_types_engine::{
    CancunPayloadFields, ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3,
    PraguePayloadFields,
};
use base_flashtypes::{ExecutionPayloadBaseV1, Flashblock};
use op_alloy_rpc_types_engine::{
    OpExecutionPayload, OpExecutionPayloadSidecar, OpExecutionPayloadV4,
};
use reth_evm::op_revm::L1BlockInfo;
use reth_optimism_primitives::OpBlock;

use crate::{ExecutionError, ProtocolError, Result};

/// Result of assembling a block from flashblocks.
#[derive(Debug, Clone)]
pub struct AssembledBlock {
    /// The reconstructed OP block.
    pub block: OpBlock,
    /// The base payload data from the first flashblock.
    pub base: ExecutionPayloadBaseV1,
    /// The flashblocks used to assemble this block.
    pub flashblocks: Vec<Flashblock>,
    /// The sealed header for this block.
    pub header: Sealed<Header>,
}

impl AssembledBlock {
    /// Extracts L1 block info from the assembled block's body.
    ///
    /// This extracts the L1 attributes deposited transaction data from the
    /// block body, which contains information about the L1 origin.
    pub fn l1_block_info(&self) -> Result<L1BlockInfo> {
        reth_optimism_evm::extract_l1_info(&self.block.body)
            .map_err(|e| ExecutionError::L1BlockInfo(e.to_string()).into())
    }
}

/// Assembles blocks from flashblocks.
///
/// This component handles the reconstruction of complete blocks from
/// a sequence of flashblocks, extracting transactions, withdrawals,
/// and building the execution payload.
#[derive(Debug, Default)]
pub struct BlockAssembler;

impl BlockAssembler {
    /// Creates a new block assembler.
    pub const fn new() -> Self {
        Self
    }

    /// Assembles a complete block from a slice of flashblocks.
    ///
    /// # Arguments
    /// * `flashblocks` - A slice of flashblocks for a single block number.
    ///
    /// # Returns
    /// An [`AssembledBlock`] containing the reconstructed block and metadata.
    ///
    /// # Errors
    /// Returns an error if:
    /// - The flashblocks slice is empty
    /// - The first flashblock is missing its base payload
    /// - Block conversion fails
    pub fn assemble(flashblocks: &[Flashblock]) -> Result<AssembledBlock> {
        let first = flashblocks.first().ok_or(ProtocolError::EmptyFlashblocks)?;
        let base = first.base.clone().ok_or(ProtocolError::MissingBase)?;
        let latest_flashblock = flashblocks.last().ok_or(ProtocolError::EmptyFlashblocks)?;

        let transactions: Vec<Bytes> = flashblocks
            .iter()
            .flat_map(|flashblock| flashblock.diff.transactions.clone())
            .collect();

        let withdrawals: Vec<Withdrawal> =
            flashblocks.iter().flat_map(|flashblock| flashblock.diff.withdrawals.clone()).collect();

        // OpExecutionPayloadV4 sets withdrawals_root directly instead of computing from list.
        let execution_payload = OpExecutionPayloadV4 {
            payload_inner: ExecutionPayloadV3 {
                blob_gas_used: latest_flashblock.diff.blob_gas_used.unwrap_or_default(),
                excess_blob_gas: 0,
                payload_inner: ExecutionPayloadV2 {
                    withdrawals,
                    payload_inner: ExecutionPayloadV1 {
                        parent_hash: base.parent_hash,
                        fee_recipient: base.fee_recipient,
                        state_root: latest_flashblock.diff.state_root,
                        receipts_root: latest_flashblock.diff.receipts_root,
                        logs_bloom: latest_flashblock.diff.logs_bloom,
                        prev_randao: base.prev_randao,
                        block_number: base.block_number,
                        gas_limit: base.gas_limit,
                        gas_used: latest_flashblock.diff.gas_used,
                        timestamp: base.timestamp,
                        extra_data: base.extra_data.clone(),
                        base_fee_per_gas: base.base_fee_per_gas,
                        block_hash: latest_flashblock.diff.block_hash,
                        transactions,
                    },
                },
            },
            withdrawals_root: latest_flashblock.diff.withdrawals_root,
        };

        // Create sidecar with fields passed separately to Engine API
        let sidecar = OpExecutionPayloadSidecar::v4(
            CancunPayloadFields {
                parent_beacon_block_root: base.parent_beacon_block_root,
                versioned_hashes: vec![],
            },
            PraguePayloadFields::new(EMPTY_REQUESTS_HASH),
        );

        let block: OpBlock = OpExecutionPayload::V4(execution_payload)
            .try_into_block_with_sidecar(&sidecar)
            .map_err(|e| ExecutionError::BlockConversion(e.to_string()))?;

        // Zero block hash for flashblocks since the final hash isn't known yet
        let sealed_header = block.header.clone().seal(B256::ZERO);

        Ok(AssembledBlock { block, base, flashblocks: flashblocks.to_vec(), header: sealed_header })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bloom, U256};
    use alloy_rpc_types_engine::PayloadId;
    use base_flashtypes::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Metadata};

    use super::*;
    use crate::ProtocolError;

    fn create_test_flashblock(index: u64, with_base: bool) -> Flashblock {
        Flashblock {
            payload_id: PayloadId::default(),
            index,
            base: if with_base {
                Some(ExecutionPayloadBaseV1 {
                    parent_beacon_block_root: B256::ZERO,
                    parent_hash: B256::ZERO,
                    fee_recipient: Address::ZERO,
                    prev_randao: B256::ZERO,
                    block_number: 100,
                    gas_limit: 30_000_000,
                    timestamp: 1700000000,
                    extra_data: Bytes::default(),
                    base_fee_per_gas: U256::from(1000000000u64),
                })
            } else {
                None
            },
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::default(),
                gas_used: 21000,
                block_hash: B256::ZERO,
                transactions: vec![],
                withdrawals: vec![],
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: Metadata { block_number: 100 },
        }
    }

    #[test]
    fn test_assemble_single_flashblock() {
        let flashblocks = vec![create_test_flashblock(0, true)];

        let result = BlockAssembler::assemble(&flashblocks);
        assert!(result.is_ok());

        let assembled = result.unwrap();
        assert_eq!(assembled.base.block_number, 100);
        assert_eq!(assembled.flashblocks.len(), 1);
    }

    #[test]
    fn test_assemble_multiple_flashblocks() {
        let flashblocks = vec![
            create_test_flashblock(0, true),
            create_test_flashblock(1, false),
            create_test_flashblock(2, false),
        ];

        let result = BlockAssembler::assemble(&flashblocks);
        assert!(result.is_ok());

        let assembled = result.unwrap();
        assert_eq!(assembled.flashblocks.len(), 3);
    }

    #[test]
    fn test_assemble_propagates_blob_gas_used_from_latest_flashblock() {
        let mut fb0 = create_test_flashblock(0, true);
        fb0.diff.blob_gas_used = Some(10);

        let mut fb1 = create_test_flashblock(1, false);
        fb1.diff.blob_gas_used = Some(42_000);

        let assembled = BlockAssembler::assemble(&[fb0, fb1]).unwrap();
        assert_eq!(assembled.block.header.blob_gas_used, Some(42_000));
    }

    #[test]
    fn test_assemble_empty_flashblocks_fails() {
        let flashblocks: Vec<Flashblock> = vec![];
        let result = BlockAssembler::assemble(&flashblocks);
        assert!(matches!(
            result,
            Err(crate::StateProcessorError::Protocol(ProtocolError::EmptyFlashblocks))
        ));
    }

    #[test]
    fn test_assemble_missing_base_fails() {
        let flashblocks = vec![create_test_flashblock(0, false)]; // No base

        let result = BlockAssembler::assemble(&flashblocks);
        assert!(matches!(
            result,
            Err(crate::StateProcessorError::Protocol(ProtocolError::MissingBase))
        ));
    }
}
