use alloc::sync::Arc;

use alloy_consensus::{
    Block, BlockBody, EMPTY_OMMER_ROOT_HASH, Header, TxReceipt, constants::EMPTY_WITHDRAWALS,
    proofs,
};
use alloy_eips::{eip7685::EMPTY_REQUESTS_HASH, merge::BEACON_NONCE};
use alloy_evm::block::BlockExecutorFactory;
use alloy_primitives::logs_bloom;
use base_alloy_evm::OpBlockExecutionCtx;
use base_execution_consensus::{calculate_receipt_root_no_memo_optimism, isthmus};
use base_execution_forks::OpHardforks;
use base_execution_primitives::DepositReceipt;
use reth_evm::execute::{BlockAssembler, BlockAssemblerInput};
use reth_execution_errors::BlockExecutionError;
use reth_execution_types::BlockExecutionResult;
use reth_primitives_traits::{Receipt, SignedTransaction};
use revm::context::Block as _;

/// Block builder for Optimism.
#[derive(Debug)]
pub struct OpBlockAssembler<ChainSpec> {
    chain_spec: Arc<ChainSpec>,
}

impl<ChainSpec> OpBlockAssembler<ChainSpec> {
    /// Creates a new [`OpBlockAssembler`].
    pub const fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self { chain_spec }
    }
}

impl<ChainSpec: OpHardforks> OpBlockAssembler<ChainSpec> {
    /// Builds a block for `input` without any bounds on header `H`.
    pub fn assemble_block<
        F: for<'a> BlockExecutorFactory<
                ExecutionCtx<'a>: Into<OpBlockExecutionCtx>,
                Transaction: SignedTransaction,
                Receipt: Receipt + DepositReceipt,
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
        let receipts_root =
            calculate_receipt_root_no_memo_optimism(receipts, &self.chain_spec, timestamp);
        let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| r.logs()));

        let mut requests_hash = None;

        let withdrawals_root =
            if OpHardforks::is_isthmus_active_at_timestamp(&*self.chain_spec, timestamp) {
                // always empty requests hash post isthmus
                requests_hash = Some(EMPTY_REQUESTS_HASH);

                // withdrawals root field in block header is used for storage root of L2 predeploy
                // `l2tol1-message-passer`
                Some(
                    isthmus::withdrawals_root(bundle_state, state_provider)
                        .map_err(BlockExecutionError::other)?,
                )
            } else if OpHardforks::is_canyon_active_at_timestamp(&*self.chain_spec, timestamp) {
                Some(EMPTY_WITHDRAWALS)
            } else {
                None
            };

        let (excess_blob_gas, blob_gas_used) =
            if OpHardforks::is_jovian_active_at_timestamp(&*self.chain_spec, timestamp) {
                // In jovian, we're using the blob gas used field to store the current da
                // footprint's value.
                (Some(0), Some(*blob_gas_used))
            } else if OpHardforks::is_ecotone_active_at_timestamp(&*self.chain_spec, timestamp) {
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
        };

        Ok(Block::new(
            header,
            BlockBody {
                transactions,
                ommers: Default::default(),
                withdrawals: OpHardforks::is_canyon_active_at_timestamp(
                    &*self.chain_spec,
                    timestamp,
                )
                .then(Default::default),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Receipt;
    use alloy_primitives::{Address, B256, Bytes, Log, LogData, b256, hex};
    use base_alloy_consensus::OpDepositReceipt;
    use base_execution_chainspec::BASE_MAINNET;
    use base_execution_consensus::calculate_receipt_root_no_memo_optimism;
    use base_execution_primitives::OpReceipt;

    fn parse_receipts(json: &[serde_json::Value]) -> Vec<OpReceipt> {
        json.iter()
            .map(|v| {
                let tx_type = v["type"].as_str().unwrap();
                let status = {
                    let s = v["status"].as_str().unwrap();
                    u64::from_str_radix(&s[2..], 16).unwrap() != 0
                };
                let cumulative_gas_used = {
                    let s = v["cumulativeGasUsed"].as_str().unwrap();
                    u64::from_str_radix(&s[2..], 16).unwrap()
                };
                let logs: Vec<Log> = v["logs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|log| {
                        let address: Address =
                            log["address"].as_str().unwrap().parse().unwrap();
                        let topics: Vec<B256> = log["topics"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|t| t.as_str().unwrap().parse().unwrap())
                            .collect();
                        let data_hex = log["data"].as_str().unwrap_or("0x");
                        let data = if data_hex == "0x" {
                            Bytes::new()
                        } else {
                            Bytes::from(hex::decode(&data_hex[2..]).unwrap())
                        };
                        Log { address, data: LogData::new_unchecked(topics, data) }
                    })
                    .collect();

                let receipt = Receipt { status: status.into(), cumulative_gas_used, logs };
                match tx_type {
                    "0x7e" => {
                        let deposit_nonce = v["depositNonce"]
                            .as_str()
                            .map(|s| u64::from_str_radix(&s[2..], 16).unwrap());
                        let deposit_receipt_version = v["depositReceiptVersion"]
                            .as_str()
                            .map(|s| u64::from_str_radix(&s[2..], 16).unwrap());
                        OpReceipt::Deposit(OpDepositReceipt {
                            inner: receipt,
                            deposit_nonce,
                            deposit_receipt_version,
                        })
                    }
                    "0x2" => OpReceipt::Eip1559(receipt),
                    "0x1" => OpReceipt::Eip2930(receipt),
                    _ => OpReceipt::Legacy(receipt),
                }
            })
            .collect()
    }

    /// Regression test for Base mainnet block 42969982.
    ///
    /// The node produced a wrong receipt root for this unsafe payload:
    ///   got:      0xae38393be3eed10b594221c7f6668f40c98489cae98eda55447cf36074204bfb
    ///   expected: 0x0381a86904da6c6fa3def4c47be5e63e3a88eff6eb96131995ad174426a20062
    ///
    /// This exercises `calculate_receipt_root_no_memo_optimism` at the exact call site used
    /// during block assembly (`build.rs:61-62`) with the on-chain receipts for that block,
    /// ensuring any regression in this path is caught immediately.
    #[test]
    fn receipt_root_base_mainnet_block_42969982() {
        let json: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../consensus/src/testdata/base_mainnet_42969982_receipts.json"
        ))
        .unwrap();

        let receipts = parse_receipts(&json);

        // Block timestamp: 0x69a9b3df
        let root = calculate_receipt_root_no_memo_optimism(
            &receipts,
            BASE_MAINNET.as_ref(),
            0x69a9b3df_u64,
        );
        assert_eq!(
            root,
            b256!("0x0381a86904da6c6fa3def4c47be5e63e3a88eff6eb96131995ad174426a20062"),
        );
    }
}

impl<ChainSpec> Clone for OpBlockAssembler<ChainSpec> {
    fn clone(&self) -> Self {
        Self { chain_spec: Arc::clone(&self.chain_spec) }
    }
}

impl<F, ChainSpec> BlockAssembler<F> for OpBlockAssembler<ChainSpec>
where
    ChainSpec: OpHardforks,
    F: for<'a> BlockExecutorFactory<
            ExecutionCtx<'a> = OpBlockExecutionCtx,
            Transaction: SignedTransaction,
            Receipt: Receipt + DepositReceipt,
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
