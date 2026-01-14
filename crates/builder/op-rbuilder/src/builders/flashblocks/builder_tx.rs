use core::fmt::Debug;

use alloy_eips::Encodable2718;
use alloy_evm::{Database, Evm};
use alloy_op_evm::OpEvm;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_eth::TransactionInput;
use alloy_sol_types::{SolCall, SolEvent, sol};
use op_alloy_rpc_types::OpTransactionRequest;
use reth_evm::{ConfigureEvm, precompiles::PrecompilesMap};
use reth_revm::State;
use revm::{DatabaseRef, inspector::NoOpInspector};
use tracing::warn;

use crate::{
    builders::{
        BuilderTransactionCtx, BuilderTransactionError, BuilderTransactions,
        SimulationSuccessResult,
        builder_tx::BuilderTxBase,
        context::OpPayloadBuilderCtx,
        flashblocks::payload::{FlashblocksExecutionInfo, FlashblocksExtraCtx},
        get_nonce,
    },
    tx_signer::Signer,
};

sol!(
    // From https://github.com/Uniswap/flashblocks_number_contract/blob/main/src/FlashblockNumber.sol
    #[sol(rpc, abi)]
    #[derive(Debug)]
    interface IFlashblockNumber {
        uint256 public flashblockNumber;

        function incrementFlashblockNumber() external;

        function permitIncrementFlashblockNumber(uint256 currentFlashblockNumber, bytes memory signature) external;

        function computeStructHash(uint256 currentFlashblockNumber) external pure returns (bytes32);

        function hashTypedDataV4(bytes32 structHash) external view returns (bytes32);


        // @notice Emitted when flashblock index is incremented
        // @param newFlashblockIndex The new flashblock index (0-indexed within each L2 block)
        event FlashblockIncremented(uint256 newFlashblockIndex);

        /// -----------------------------------------------------------------------
        /// Errors
        /// -----------------------------------------------------------------------
        error NonBuilderAddress(address addr);
        error MismatchedFlashblockNumber(uint256 expectedFlashblockNumber, uint256 actualFlashblockNumber);
    }
);

// This will be the end of block transaction of a regular block
#[derive(Debug, Clone)]
pub(super) struct FlashblocksBuilderTx {
    pub base_builder_tx: BuilderTxBase<FlashblocksExtraCtx>,
}

impl FlashblocksBuilderTx {
    pub(super) const fn new(signer: Option<Signer>) -> Self {
        let base_builder_tx = BuilderTxBase::new(signer);
        Self { base_builder_tx }
    }
}

impl BuilderTransactions<FlashblocksExtraCtx, FlashblocksExecutionInfo> for FlashblocksBuilderTx {
    fn simulate_builder_txs(
        &self,
        ctx: &OpPayloadBuilderCtx<FlashblocksExtraCtx>,
        db: &mut State<impl Database + DatabaseRef>,
    ) -> Result<Vec<BuilderTransactionCtx>, BuilderTransactionError> {
        let mut builder_txs = Vec::<BuilderTransactionCtx>::new();

        if ctx.is_first_flashblock() {
            let flashblocks_builder_tx = self.base_builder_tx.simulate_builder_tx(ctx, &mut *db)?;
            builder_txs.extend(flashblocks_builder_tx);
        }

        if ctx.is_last_flashblock() {
            let base_tx = self.base_builder_tx.simulate_builder_tx(ctx, &mut *db)?;
            builder_txs.extend(base_tx);
        }
        Ok(builder_txs)
    }
}

// This will be the end of block transaction of a regular block
#[derive(Debug, Clone)]
pub(super) struct FlashblocksNumberBuilderTx {
    pub signer: Signer,
    pub flashblock_number_address: Address,
    pub base_builder_tx: BuilderTxBase<FlashblocksExtraCtx>,
}

impl FlashblocksNumberBuilderTx {
    pub(super) const fn new(signer: Signer, flashblock_number_address: Address) -> Self {
        let base_builder_tx = BuilderTxBase::new(Some(signer));
        Self { signer, flashblock_number_address, base_builder_tx }
    }

    fn signed_increment_flashblocks_tx(
        &self,
        ctx: &OpPayloadBuilderCtx<FlashblocksExtraCtx>,
        evm: &mut OpEvm<impl Database + DatabaseRef, NoOpInspector, PrecompilesMap>,
    ) -> Result<BuilderTransactionCtx, BuilderTransactionError> {
        let calldata = IFlashblockNumber::incrementFlashblockNumberCall {};
        self.increment_flashblocks_tx(calldata, ctx, evm)
    }

    fn increment_flashblocks_tx<T: SolCall + Clone>(
        &self,
        calldata: T,
        ctx: &OpPayloadBuilderCtx<FlashblocksExtraCtx>,
        evm: &mut OpEvm<impl Database + DatabaseRef, NoOpInspector, PrecompilesMap>,
    ) -> Result<BuilderTransactionCtx, BuilderTransactionError> {
        let SimulationSuccessResult { gas_used, .. } = self.simulate_flashblocks_call(
            calldata.clone(),
            vec![IFlashblockNumber::FlashblockIncremented::SIGNATURE_HASH],
            ctx,
            evm,
        )?;
        let signed_tx = self.sign_tx(
            self.flashblock_number_address,
            self.signer,
            gas_used,
            calldata.abi_encode().into(),
            ctx,
            evm.db_mut(),
        )?;
        let da_size =
            op_alloy_flz::tx_estimated_size_fjord_bytes(signed_tx.encoded_2718().as_slice());
        Ok(BuilderTransactionCtx { signed_tx, gas_used, da_size, is_top_of_block: true })
    }

    fn simulate_flashblocks_call<T: SolCall>(
        &self,
        calldata: T,
        expected_logs: Vec<B256>,
        ctx: &OpPayloadBuilderCtx<FlashblocksExtraCtx>,
        evm: &mut OpEvm<impl Database + DatabaseRef, NoOpInspector, PrecompilesMap>,
    ) -> Result<SimulationSuccessResult<T>, BuilderTransactionError> {
        let tx_req = OpTransactionRequest::default()
            .gas_limit(ctx.block_gas_limit())
            .max_fee_per_gas(ctx.base_fee().into())
            .to(self.flashblock_number_address)
            .from(self.signer.address)
            .nonce(get_nonce(evm.db(), self.signer.address)?)
            .input(TransactionInput::new(calldata.abi_encode().into()));
        self.simulate_call::<T, IFlashblockNumber::IFlashblockNumberErrors>(
            tx_req,
            expected_logs,
            evm,
        )
    }
}

impl BuilderTransactions<FlashblocksExtraCtx, FlashblocksExecutionInfo>
    for FlashblocksNumberBuilderTx
{
    fn simulate_builder_txs(
        &self,
        ctx: &OpPayloadBuilderCtx<FlashblocksExtraCtx>,
        db: &mut State<impl Database + DatabaseRef>,
    ) -> Result<Vec<BuilderTransactionCtx>, BuilderTransactionError> {
        let mut builder_txs = Vec::<BuilderTransactionCtx>::new();

        if ctx.is_first_flashblock() {
            // fallback block builder tx
            builder_txs.extend(self.base_builder_tx.simulate_builder_tx(ctx, &mut *db)?);
        } else {
            // we increment the flashblock number for the next flashblock so we don't increment in the last flashblock
            let mut evm = ctx.evm_config.evm_with_env(&mut *db, ctx.evm_env.clone());
            evm.modify_cfg(|cfg| {
                cfg.disable_balance_check = true;
                cfg.disable_block_gas_limit = true;
            });

            let flashblocks_num_tx = self.signed_increment_flashblocks_tx(ctx, &mut evm);

            let tx = match flashblocks_num_tx {
                Ok(tx) => Some(tx),
                Err(e) => {
                    warn!(target: "builder_tx", error = ?e, "flashblocks number contract tx simulation failed, defaulting to fallback builder tx");
                    self.base_builder_tx
                        .simulate_builder_tx(ctx, &mut *db)?
                        .map(|tx| tx.set_top_of_block())
                }
            };

            builder_txs.extend(tx);
        }

        Ok(builder_txs)
    }
}
