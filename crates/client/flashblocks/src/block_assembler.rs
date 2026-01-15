//! Block assembly from flashblocks.
//!
//! This module provides the [`BlockAssembler`] which reconstructs blocks from flashblocks.

use alloy_consensus::{Header, Sealed};
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types::Withdrawal;
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
use base_flashtypes::{ExecutionPayloadBaseV1, Flashblock};
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
    /// * `flashblocks` - A slice of flashblock references for a single block number.
    ///
    /// # Returns
    /// An [`AssembledBlock`] containing the reconstructed block and metadata.
    ///
    /// # Errors
    /// Returns an error if:
    /// - The flashblocks slice is empty
    /// - The first flashblock is missing its base payload
    /// - Block conversion fails
    pub fn assemble(flashblocks: &[&Flashblock]) -> Result<AssembledBlock> {
        let base = flashblocks
            .first()
            .ok_or(ProtocolError::EmptyFlashblocks)?
            .base
            .clone()
            .ok_or(ProtocolError::MissingBase)?;

        let latest_flashblock = flashblocks.last().cloned().ok_or(ProtocolError::EmptyFlashblocks)?;

        let transactions: Vec<Bytes> = flashblocks
            .iter()
            .flat_map(|flashblock| flashblock.diff.transactions.clone())
            .collect();

        let withdrawals: Vec<Withdrawal> = flashblocks
            .iter()
            .flat_map(|flashblock| flashblock.diff.withdrawals.clone())
            .collect();

        let execution_payload = ExecutionPayloadV3 {
            blob_gas_used: 0,
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
        };

        let block: OpBlock = execution_payload
            .try_into_block()
            .map_err(|e| ExecutionError::BlockConversion(e.to_string()))?;

        let block_header = block.header.clone();
        // Zero block hash for flashblocks since the final hash isn't known yet
        let sealed_header = block_header.seal(B256::ZERO);

        let flashblocks_owned: Vec<Flashblock> =
            flashblocks.iter().map(|&fb| fb.clone()).collect();

        Ok(AssembledBlock { block, base, flashblocks: flashblocks_owned, header: sealed_header })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bloom, U256};
    use alloy_rpc_types_engine::PayloadId;
    use base_flashtypes::{ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Metadata};

    use super::*;

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
        let fb = create_test_flashblock(0, true);
        let flashblocks = vec![&fb];

        let result = BlockAssembler::assemble(&flashblocks);
        assert!(result.is_ok());

        let assembled = result.unwrap();
        assert_eq!(assembled.base.block_number, 100);
        assert_eq!(assembled.flashblocks.len(), 1);
    }

    #[test]
    fn test_assemble_multiple_flashblocks() {
        let fb0 = create_test_flashblock(0, true);
        let fb1 = create_test_flashblock(1, false);
        let fb2 = create_test_flashblock(2, false);
        let flashblocks = vec![&fb0, &fb1, &fb2];

        let result = BlockAssembler::assemble(&flashblocks);
        assert!(result.is_ok());

        let assembled = result.unwrap();
        assert_eq!(assembled.flashblocks.len(), 3);
    }

    #[test]
    fn test_assemble_empty_flashblocks_fails() {
        let flashblocks: Vec<&Flashblock> = vec![];
        let result = BlockAssembler::assemble(&flashblocks);
        assert!(result.is_err());
    }

    #[test]
    fn test_assemble_missing_base_fails() {
        let fb = create_test_flashblock(0, false); // No base
        let flashblocks = vec![&fb];

        let result = BlockAssembler::assemble(&flashblocks);
        assert!(result.is_err());
    }
}
