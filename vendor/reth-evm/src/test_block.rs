//! Assembly of executed blocks for generic storage and engine test fixtures.

use std::sync::Arc;

use alloy_consensus::{
    Block, BlockBody, BlockHeader, EMPTY_OMMER_ROOT_HASH, Header, TxReceipt, proofs,
};
use alloy_eips::{eip4895::Withdrawals, merge::BEACON_NONCE};
use alloy_evm::{block::BlockExecutorFactory, eth::EthBlockExecutionCtx};
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_primitives_traits::{Receipt, SignedTransaction, logs_bloom};
use revm::context::Block as _;

use crate::execute::{BlockAssembler, BlockAssemblerInput, BlockExecutionError};

/// Assembles test blocks from real execution output.
#[derive(Debug, Clone)]
pub struct TestBlockAssembler {
    /// Fork schedule used to encode test block headers.
    pub chain_spec: Arc<ChainSpec>,
}

impl<F> BlockAssembler<F> for TestBlockAssembler
where
    F: for<'a> BlockExecutorFactory<
            ExecutionCtx<'a> = EthBlockExecutionCtx<'a>,
            Transaction: SignedTransaction,
            Receipt: Receipt,
        >,
{
    type Block = Block<F::Transaction>;

    fn assemble_block(
        &self,
        input: BlockAssemblerInput<'_, '_, F>,
    ) -> Result<Self::Block, BlockExecutionError> {
        let timestamp = input.evm_env.block_env.timestamp().saturating_to();
        let number = input.evm_env.block_env.number().saturating_to();
        let withdrawals = self.chain_spec.is_shanghai_active_at_timestamp(timestamp).then(|| {
            Withdrawals::new(
                input.execution_ctx.withdrawals.map(|w| w.into_owned()).unwrap_or_default(),
            )
        });
        let cancun = self.chain_spec.is_cancun_active_at_timestamp(timestamp);
        let header = Header {
            parent_hash: input.execution_ctx.parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: input.evm_env.block_env.beneficiary(),
            state_root: input.state_root,
            transactions_root: proofs::calculate_transaction_root(&input.transactions),
            receipts_root: proofs::calculate_receipt_root(
                &input.output.receipts.iter().map(TxReceipt::with_bloom_ref).collect::<Vec<_>>(),
            ),
            withdrawals_root: withdrawals
                .as_deref()
                .map(|withdrawals| proofs::calculate_withdrawals_root(withdrawals)),
            logs_bloom: logs_bloom(input.output.receipts.iter().flat_map(TxReceipt::logs)),
            timestamp,
            mix_hash: input.evm_env.block_env.prevrandao().unwrap_or_default(),
            nonce: BEACON_NONCE.into(),
            base_fee_per_gas: self
                .chain_spec
                .is_london_active_at_block(number)
                .then(|| input.evm_env.block_env.basefee()),
            number,
            gas_limit: input.evm_env.block_env.gas_limit(),
            difficulty: input.evm_env.block_env.difficulty(),
            gas_used: input.output.gas_used,
            extra_data: input.execution_ctx.extra_data,
            parent_beacon_block_root: input.execution_ctx.parent_beacon_block_root,
            blob_gas_used: cancun.then_some(input.output.blob_gas_used),
            excess_blob_gas: cancun.then(|| {
                input
                    .parent
                    .maybe_next_block_excess_blob_gas(
                        self.chain_spec.blob_params_at_timestamp(timestamp),
                    )
                    .unwrap_or_default()
            }),
            requests_hash: self
                .chain_spec
                .is_prague_active_at_timestamp(timestamp)
                .then(|| input.output.requests.requests_hash()),
            block_access_list_hash: input.block_access_list_hash,
            slot_number: input.execution_ctx.slot_number,
        };
        Ok(Block {
            header,
            body: BlockBody { transactions: input.transactions, ommers: Vec::new(), withdrawals },
        })
    }
}
