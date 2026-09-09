//! Consensus fixture for testing storage and engine behavior with shared validation rules.
//! This checks headers, bodies, and execution results without implementing a network's consensus
//! or requiring a full execution node.

use std::sync::Arc;

use alloy_hardforks::EthereumHardforks;
use alloy_primitives::{B256, Bloom};
use base_common_types_chain::{
    BaseReceipt, BlockHeader as _, TxReceipt, proofs::calculate_receipt_root,
};
use base_execution_chainspec::BaseChainSpec;
use reth_execution_types::BlockExecutionResult;
use reth_primitives_traits::{
    GotExpected, RecoveredBlock, SealedBlock, SealedHeader, receipt::gas_spent_by_transactions,
};

use crate::{
    ConsensusError, ReceiptRootBloom,
    common_validation::{
        validate_against_parent_hash_number, validate_against_parent_timestamp,
        validate_block_pre_execution, validate_body_against_header, validate_header_base_fee,
        validate_header_extra_data, validate_header_gas,
    },
};

/// Shared validation fixture used by storage and engine tests.
#[derive(Debug, Clone)]
pub struct EthereumTestConsensus {
    /// Fork schedule for the test's execution rules.
    pub chain_spec: Arc<BaseChainSpec>,
}

impl EthereumTestConsensus {
    /// Creates a validation fixture.
    pub const fn new(chain_spec: Arc<BaseChainSpec>) -> Self {
        Self { chain_spec }
    }
}

impl EthereumTestConsensus {
    pub fn validate_header(&self, header: &SealedHeader) -> Result<(), ConsensusError> {
        validate_header_extra_data(header.header(), 32)?;
        validate_header_gas(header.header())?;
        validate_header_base_fee(header.header(), &self.chain_spec)
    }

    pub fn validate_header_against_parent(
        &self,
        header: &SealedHeader,
        parent: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        validate_against_parent_hash_number(header.header(), parent)?;
        validate_against_parent_timestamp(header.header(), parent.header())
    }
}

impl EthereumTestConsensus {
    pub fn validate_body_against_header(
        &self,
        body: &base_common_types_chain::BaseBlockBody,
        header: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        validate_body_against_header(body, header.header())
    }

    pub fn validate_block_pre_execution(&self, block: &SealedBlock) -> Result<(), ConsensusError> {
        validate_block_pre_execution(block, &self.chain_spec)
    }
}

impl EthereumTestConsensus {
    pub fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock,
        result: &BlockExecutionResult<BaseReceipt>,
        receipt_root_bloom: Option<ReceiptRootBloom>,
        block_access_list_hash: Option<B256>,
    ) -> Result<(), ConsensusError> {
        if block.header().gas_used() != result.gas_used {
            return Err(ConsensusError::BlockGasUsed {
                gas: GotExpected::new(result.gas_used, block.header().gas_used()),
                gas_spent_by_tx: gas_spent_by_transactions(&result.receipts),
            });
        }
        if self.chain_spec.is_byzantium_active_at_block(block.header().number()) {
            let (root, bloom) = receipt_root_bloom.unwrap_or_else(|| {
                let receipts =
                    result.receipts.iter().map(TxReceipt::with_bloom_ref).collect::<Vec<_>>();
                let root = calculate_receipt_root(&receipts);
                let bloom = receipts.iter().fold(Bloom::ZERO, |bloom, r| bloom | r.bloom_ref());
                (root, bloom)
            });
            if root != block.header().receipts_root() {
                return Err(ConsensusError::BodyReceiptRootDiff(
                    GotExpected::new(root, block.header().receipts_root()).into(),
                ));
            }
            if bloom != block.header().logs_bloom() {
                return Err(ConsensusError::BodyBloomLogDiff(
                    GotExpected::new(bloom, block.header().logs_bloom()).into(),
                ));
            }
        }
        if let Some(actual) = block_access_list_hash
            && self.chain_spec.is_amsterdam_active_at_timestamp(block.header().timestamp())
            && actual != block.header().block_access_list_hash().unwrap_or_default()
        {
            return Err(ConsensusError::BlockAccessListHashMismatch(
                GotExpected::new(
                    actual,
                    block.header().block_access_list_hash().unwrap_or_default(),
                )
                .into(),
            ));
        }
        Ok(())
    }
}
