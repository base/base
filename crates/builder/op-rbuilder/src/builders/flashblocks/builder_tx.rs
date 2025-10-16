use alloy_consensus::TxEip1559;
use alloy_eips::Encodable2718;
use alloy_evm::{Database, Evm};
use alloy_op_evm::OpEvm;
use alloy_primitives::{Address, B256, TxKind};
use alloy_sol_types::{Error, SolCall, SolEvent, SolInterface, sol};
use core::fmt::Debug;
use op_alloy_consensus::OpTypedTransaction;
use op_revm::OpHaltReason;
use reth_evm::{ConfigureEvm, precompiles::PrecompilesMap};
use reth_optimism_primitives::OpTransactionSigned;
use reth_primitives::Recovered;
use reth_provider::StateProvider;
use reth_revm::{State, database::StateProviderDatabase};
use revm::{
    context::result::{ExecutionResult, ResultAndState},
    inspector::NoOpInspector,
};
use tracing::warn;

use crate::{
    builders::{
        BuilderTransactionCtx, BuilderTransactionError, BuilderTransactions,
        builder_tx::{BuilderTxBase, get_nonce, log_exists},
        context::OpPayloadBuilderCtx,
        flashblocks::payload::FlashblocksExtraCtx,
    },
    flashtestations::builder_tx::FlashtestationsBuilderTx,
    primitives::reth::ExecutionInfo,
    tx_signer::Signer,
};

sol!(
    // From https://github.com/Uniswap/flashblocks_number_contract/blob/main/src/FlashblockNumber.sol
    #[sol(rpc, abi)]
    #[derive(Debug)]
    interface IFlashblockNumber {
        function incrementFlashblockNumber() external;

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

#[derive(Debug, thiserror::Error)]
pub(super) enum FlashblockNumberError {
    #[error("flashblocks number contract tx reverted: {0:?}")]
    Revert(IFlashblockNumber::IFlashblockNumberErrors),
    #[error("contract may be invalid, mismatch in log emitted: expected {0:?}")]
    LogMismatch(B256),
    #[error("unknown revert: {0} err: {1}")]
    Unknown(String, Error),
    #[error("halt: {0:?}")]
    Halt(OpHaltReason),
}

// This will be the end of block transaction of a regular block
#[derive(Debug, Clone)]
pub(super) struct FlashblocksBuilderTx {
    pub base_builder_tx: BuilderTxBase,
    pub flashtestations_builder_tx: Option<FlashtestationsBuilderTx>,
}

impl FlashblocksBuilderTx {
    pub(super) fn new(
        signer: Option<Signer>,
        flashtestations_builder_tx: Option<FlashtestationsBuilderTx>,
    ) -> Self {
        let base_builder_tx = BuilderTxBase::new(signer);
        Self {
            base_builder_tx,
            flashtestations_builder_tx,
        }
    }
}

impl BuilderTransactions<FlashblocksExtraCtx> for FlashblocksBuilderTx {
    fn simulate_builder_txs<Extra: Debug + Default>(
        &self,
        state_provider: impl StateProvider + Clone,
        info: &mut ExecutionInfo<Extra>,
        ctx: &OpPayloadBuilderCtx<FlashblocksExtraCtx>,
        db: &mut State<impl Database>,
    ) -> Result<Vec<BuilderTransactionCtx>, BuilderTransactionError> {
        let mut builder_txs = Vec::<BuilderTransactionCtx>::new();

        if ctx.is_first_flashblock() {
            let flashblocks_builder_tx = self.base_builder_tx.simulate_builder_tx(ctx, db)?;
            builder_txs.extend(flashblocks_builder_tx.clone());
        }

        if ctx.is_last_flashblock() {
            let base_tx = self.base_builder_tx.simulate_builder_tx(ctx, db)?;
            builder_txs.extend(base_tx.clone());

            if let Some(flashtestations_builder_tx) = &self.flashtestations_builder_tx {
                // We only include flashtestations txs in the last flashblock
                let mut simulation_state = self.simulate_builder_txs_state::<FlashblocksExtraCtx>(
                    state_provider.clone(),
                    base_tx.iter().collect(),
                    ctx,
                    db,
                )?;
                let flashtestations_builder_txs = flashtestations_builder_tx.simulate_builder_txs(
                    state_provider,
                    info,
                    ctx,
                    &mut simulation_state,
                )?;
                builder_txs.extend(flashtestations_builder_txs);
            }
        }
        Ok(builder_txs)
    }
}

// This will be the end of block transaction of a regular block
#[derive(Debug, Clone)]
pub(super) struct FlashblocksNumberBuilderTx {
    pub signer: Option<Signer>,
    pub flashblock_number_address: Address,
    pub base_builder_tx: BuilderTxBase,
    pub flashtestations_builder_tx: Option<FlashtestationsBuilderTx>,
}

impl FlashblocksNumberBuilderTx {
    pub(super) fn new(
        signer: Option<Signer>,
        flashblock_number_address: Address,
        flashtestations_builder_tx: Option<FlashtestationsBuilderTx>,
    ) -> Self {
        let base_builder_tx = BuilderTxBase::new(signer);
        Self {
            signer,
            flashblock_number_address,
            base_builder_tx,
            flashtestations_builder_tx,
        }
    }

    fn estimate_flashblock_number_tx_gas(
        &self,
        ctx: &OpPayloadBuilderCtx<FlashblocksExtraCtx>,
        evm: &mut OpEvm<
            State<StateProviderDatabase<impl StateProvider + Clone>>,
            NoOpInspector,
            PrecompilesMap,
        >,
        signer: &Signer,
        nonce: u64,
    ) -> Result<u64, BuilderTransactionError> {
        let tx = self.signed_flashblock_number_tx(ctx, ctx.block_gas_limit(), nonce, signer)?;
        let ResultAndState { result, .. } = match evm.transact(&tx) {
            Ok(res) => res,
            Err(err) => {
                return Err(BuilderTransactionError::EvmExecutionError(Box::new(err)));
            }
        };

        match result {
            ExecutionResult::Success { gas_used, logs, .. } => {
                if log_exists(
                    &logs,
                    &IFlashblockNumber::FlashblockIncremented::SIGNATURE_HASH,
                ) {
                    Ok(gas_used)
                } else {
                    Err(BuilderTransactionError::other(
                        FlashblockNumberError::LogMismatch(
                            IFlashblockNumber::FlashblockIncremented::SIGNATURE_HASH,
                        ),
                    ))
                }
            }
            ExecutionResult::Revert { output, .. } => Err(BuilderTransactionError::other(
                IFlashblockNumber::IFlashblockNumberErrors::abi_decode(&output)
                    .map(FlashblockNumberError::Revert)
                    .unwrap_or_else(|e| FlashblockNumberError::Unknown(hex::encode(output), e)),
            )),
            ExecutionResult::Halt { reason, .. } => Err(BuilderTransactionError::other(
                FlashblockNumberError::Halt(reason),
            )),
        }
    }

    fn signed_flashblock_number_tx(
        &self,
        ctx: &OpPayloadBuilderCtx<FlashblocksExtraCtx>,
        gas_limit: u64,
        nonce: u64,
        signer: &Signer,
    ) -> Result<Recovered<OpTransactionSigned>, secp256k1::Error> {
        let calldata = IFlashblockNumber::incrementFlashblockNumberCall {}.abi_encode();
        // Create the EIP-1559 transaction
        let tx = OpTypedTransaction::Eip1559(TxEip1559 {
            chain_id: ctx.chain_id(),
            nonce,
            gas_limit,
            max_fee_per_gas: ctx.base_fee().into(),
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(self.flashblock_number_address),
            input: calldata.into(),
            ..Default::default()
        });
        signer.sign_tx(tx)
    }
}

impl BuilderTransactions<FlashblocksExtraCtx> for FlashblocksNumberBuilderTx {
    fn simulate_builder_txs<Extra: Debug + Default>(
        &self,
        state_provider: impl StateProvider + Clone,
        info: &mut ExecutionInfo<Extra>,
        ctx: &OpPayloadBuilderCtx<FlashblocksExtraCtx>,
        db: &mut State<impl Database>,
    ) -> Result<Vec<BuilderTransactionCtx>, BuilderTransactionError> {
        let mut builder_txs = Vec::<BuilderTransactionCtx>::new();
        let state = StateProviderDatabase::new(state_provider.clone());
        let simulation_state = State::builder()
            .with_database(state)
            .with_cached_prestate(db.cache.clone())
            .with_bundle_update()
            .build();

        if ctx.is_first_flashblock() {
            // fallback block builder tx
            builder_txs.extend(self.base_builder_tx.simulate_builder_tx(ctx, db)?);
        } else {
            // we increment the flashblock number for the next flashblock so we don't increment in the last flashblock
            if let Some(signer) = &self.signer {
                let mut evm = ctx
                    .evm_config
                    .evm_with_env(simulation_state, ctx.evm_env.clone());
                evm.modify_cfg(|cfg| {
                    cfg.disable_balance_check = true;
                });

                let nonce = get_nonce(evm.db_mut(), signer.address)?;

                let tx = match self.estimate_flashblock_number_tx_gas(ctx, &mut evm, signer, nonce)
                {
                    Ok(gas_used) => {
                        // Due to EIP-150, 63/64 of available gas is forwarded to external calls so need to add a buffer
                        let signed_tx = self.signed_flashblock_number_tx(
                            ctx,
                            gas_used * 64 / 63,
                            nonce,
                            signer,
                        )?;

                        let da_size = op_alloy_flz::tx_estimated_size_fjord_bytes(
                            signed_tx.encoded_2718().as_slice(),
                        );
                        Some(BuilderTransactionCtx {
                            gas_used,
                            da_size,
                            signed_tx,
                            is_top_of_block: true, // number tx at top of flashblock
                        })
                    }
                    Err(e) => {
                        warn!(target: "builder_tx", error = ?e, "Flashblocks number contract tx simulation failed, defaulting to fallback builder tx");
                        self.base_builder_tx
                            .simulate_builder_tx(ctx, db)?
                            .map(|tx| tx.set_top_of_block())
                    }
                };

                builder_txs.extend(tx);
            }
        }

        if ctx.is_last_flashblock() {
            if let Some(flashtestations_builder_tx) = &self.flashtestations_builder_tx {
                let flashblocks_builder_txs = builder_txs.clone();
                let mut simulation_state = self.simulate_builder_txs_state::<FlashblocksExtraCtx>(
                    state_provider.clone(),
                    flashblocks_builder_txs.iter().collect(),
                    ctx,
                    db,
                )?;
                // We only include flashtestations txs in the last flashblock
                let flashtestations_builder_txs = flashtestations_builder_tx.simulate_builder_txs(
                    state_provider,
                    info,
                    ctx,
                    &mut simulation_state,
                )?;
                builder_txs.extend(flashtestations_builder_txs);
            }
        }

        Ok(builder_txs)
    }
}
