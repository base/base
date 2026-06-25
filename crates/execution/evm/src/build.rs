use alloc::sync::Arc;

use alloy_consensus::{
    Block, BlockBody, EMPTY_OMMER_ROOT_HASH, Header, TxReceipt, constants::EMPTY_WITHDRAWALS,
    proofs,
};
use alloy_eips::{eip7685::EMPTY_REQUESTS_HASH, merge::BEACON_NONCE};
use alloy_evm::block::BlockExecutorFactory;
use alloy_primitives::logs_bloom;
use base_common_chains::Upgrades;
use base_common_consensus::DepositReceiptExt;
use base_common_evm::BaseBlockExecutionCtx;
use base_execution_consensus::{calculate_receipt_root_no_memo, isthmus};
use reth_evm::execute::{BlockAssembler, BlockAssemblerInput};
use reth_execution_errors::BlockExecutionError;
use reth_execution_types::BlockExecutionResult;
use reth_primitives_traits::{Receipt, SignedTransaction};
use revm::context::Block as _;

/// Block builder for Base.
#[derive(Debug)]
pub struct BaseBlockAssembler<ChainSpec> {
    chain_spec: Arc<ChainSpec>,
}

impl<ChainSpec> BaseBlockAssembler<ChainSpec> {
    /// Creates a new [`BaseBlockAssembler`].
    pub const fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self { chain_spec }
    }
}

impl<ChainSpec: Upgrades> BaseBlockAssembler<ChainSpec> {
    /// Builds a block for `input` without any bounds on header `H`.
    pub fn assemble_block<
        F: for<'a> BlockExecutorFactory<
                ExecutionCtx<'a>: Into<BaseBlockExecutionCtx>,
                Transaction: SignedTransaction,
                Receipt: Receipt + DepositReceiptExt,
            >,
        H,
    >(
        &self,
        input: BlockAssemblerInput<'_, '_, F, H>,
    ) -> Result<Block<F::Transaction>, BlockExecutionError> {
        let BlockAssemblerInput {
            evm_env,
            execution_ctx: ctx,
            transactions,
            output: BlockExecutionResult { receipts, gas_used, blob_gas_used, requests: _ },
            bundle_state,
            state_root,
            state_provider,
            ..
        } = input;
        let ctx = ctx.into();

        let timestamp = evm_env.block_env.timestamp().saturating_to();

        let transactions_root = proofs::calculate_transaction_root(&transactions);
        let receipts_root = calculate_receipt_root_no_memo(receipts, &self.chain_spec, timestamp);
        let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| r.logs()));

        let mut requests_hash = None;

        let withdrawals_root =
            if Upgrades::is_isthmus_active_at_timestamp(&*self.chain_spec, timestamp) {
                // always empty requests hash post isthmus
                requests_hash = Some(EMPTY_REQUESTS_HASH);

                // withdrawals root field in block header is used for storage root of L2 predeploy
                // `l2tol1-message-passer`
                Some(
                    isthmus::withdrawals_root(bundle_state, state_provider)
                        .map_err(BlockExecutionError::other)?,
                )
            } else if Upgrades::is_canyon_active_at_timestamp(&*self.chain_spec, timestamp) {
                Some(EMPTY_WITHDRAWALS)
            } else {
                None
            };

        let (excess_blob_gas, blob_gas_used) =
            if Upgrades::is_jovian_active_at_timestamp(&*self.chain_spec, timestamp) {
                // In jovian, we're using the blob gas used field to store the current da
                // footprint's value.
                (Some(0), Some(*blob_gas_used))
            } else if Upgrades::is_ecotone_active_at_timestamp(&*self.chain_spec, timestamp) {
                (Some(0), Some(0))
            } else {
                (None, None)
            };

        let header = Header {
            parent_hash: ctx.parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: evm_env.block_env.beneficiary(),
            state_root,
            transactions_root,
            receipts_root,
            withdrawals_root,
            logs_bloom,
            timestamp,
            mix_hash: evm_env.block_env.prevrandao().unwrap_or_default(),
            nonce: BEACON_NONCE.into(),
            base_fee_per_gas: Some(evm_env.block_env.basefee()),
            number: evm_env.block_env.number().saturating_to(),
            gas_limit: evm_env.block_env.gas_limit(),
            difficulty: evm_env.block_env.difficulty(),
            gas_used: *gas_used,
            extra_data: ctx.extra_data,
            parent_beacon_block_root: ctx.parent_beacon_block_root,
            blob_gas_used,
            excess_blob_gas,
            requests_hash,
            block_access_list_hash: None,
            slot_number: None,
        };

        Ok(Block::new(
            header,
            BlockBody {
                transactions,
                ommers: Default::default(),
                withdrawals: Upgrades::is_canyon_active_at_timestamp(&*self.chain_spec, timestamp)
                    .then(Default::default),
            },
        ))
    }
}

impl<ChainSpec> Clone for BaseBlockAssembler<ChainSpec> {
    fn clone(&self) -> Self {
        Self { chain_spec: Arc::clone(&self.chain_spec) }
    }
}

impl<F, ChainSpec> BlockAssembler<F> for BaseBlockAssembler<ChainSpec>
where
    ChainSpec: Upgrades,
    F: for<'a> BlockExecutorFactory<
            ExecutionCtx<'a> = BaseBlockExecutionCtx,
            Transaction: SignedTransaction,
            Receipt: Receipt + DepositReceiptExt,
        >,
{
    type Block = Block<F::Transaction>;

    fn assemble_block(
        &self,
        input: BlockAssemblerInput<'_, '_, F>,
    ) -> Result<Self::Block, BlockExecutionError> {
        self.assemble_block(input)
    }
}
