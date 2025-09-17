use alloy_consensus::TxEip1559;
use alloy_eips::{Encodable2718, eip7623::TOTAL_COST_FLOOR_PER_TOKEN};
use alloy_evm::Database;
use alloy_primitives::{Address, TxKind};
use core::fmt::Debug;
use op_alloy_consensus::OpTypedTransaction;
use reth_optimism_primitives::OpTransactionSigned;
use reth_primitives::Recovered;
use reth_provider::StateProvider;
use reth_revm::State;

use crate::{
    builders::{
        BuilderTransactionCtx, BuilderTransactionError, BuilderTransactions,
        context::OpPayloadBuilderCtx, flashblocks::payload::FlashblocksExtraCtx,
    },
    flashtestations::service::FlashtestationsBuilderTx,
    primitives::reth::ExecutionInfo,
    tx_signer::Signer,
};

// This will be the end of block transaction of a regular block
#[derive(Debug, Clone)]
pub(super) struct FlashblocksBuilderTx {
    pub signer: Option<Signer>,
    pub flashtestations_builder_tx: Option<FlashtestationsBuilderTx>,
}

impl FlashblocksBuilderTx {
    pub(super) fn new(
        signer: Option<Signer>,
        flashtestations_builder_tx: Option<FlashtestationsBuilderTx>,
    ) -> Self {
        Self {
            signer,
            flashtestations_builder_tx,
        }
    }

    pub(super) fn simulate_builder_tx<ExtraCtx: Debug + Default>(
        &self,
        ctx: &OpPayloadBuilderCtx<ExtraCtx>,
        db: &mut State<impl Database>,
    ) -> Result<Option<BuilderTransactionCtx>, BuilderTransactionError> {
        match self.signer {
            Some(signer) => {
                let message: Vec<u8> = format!("Block Number: {}", ctx.block_number()).into_bytes();
                let gas_used = self.estimate_builder_tx_gas(&message);
                let signed_tx = self.signed_builder_tx(ctx, db, signer, gas_used, message)?;
                let da_size = op_alloy_flz::tx_estimated_size_fjord_bytes(
                    signed_tx.encoded_2718().as_slice(),
                );
                Ok(Some(BuilderTransactionCtx {
                    gas_used,
                    da_size,
                    signed_tx,
                }))
            }
            None => Ok(None),
        }
    }

    fn estimate_builder_tx_gas(&self, input: &[u8]) -> u64 {
        // Count zero and non-zero bytes
        let (zero_bytes, nonzero_bytes) = input.iter().fold((0, 0), |(zeros, nonzeros), &byte| {
            if byte == 0 {
                (zeros + 1, nonzeros)
            } else {
                (zeros, nonzeros + 1)
            }
        });

        // Calculate gas cost (4 gas per zero byte, 16 gas per non-zero byte)
        let zero_cost = zero_bytes * 4;
        let nonzero_cost = nonzero_bytes * 16;

        // Tx gas should be not less than floor gas https://eips.ethereum.org/EIPS/eip-7623
        let tokens_in_calldata = zero_bytes + nonzero_bytes * 4;
        let floor_gas = 21_000 + tokens_in_calldata * TOTAL_COST_FLOOR_PER_TOKEN;

        std::cmp::max(zero_cost + nonzero_cost + 21_000, floor_gas)
    }

    fn signed_builder_tx<ExtraCtx: Debug + Default>(
        &self,
        ctx: &OpPayloadBuilderCtx<ExtraCtx>,
        db: &mut State<impl Database>,
        signer: Signer,
        gas_used: u64,
        message: Vec<u8>,
    ) -> Result<Recovered<OpTransactionSigned>, BuilderTransactionError> {
        let nonce = db
            .load_cache_account(signer.address)
            .map(|acc| acc.account_info().unwrap_or_default().nonce)
            .map_err(|_| BuilderTransactionError::AccountLoadFailed(signer.address))?;

        // Create the EIP-1559 transaction
        let tx = OpTypedTransaction::Eip1559(TxEip1559 {
            chain_id: ctx.chain_id(),
            nonce,
            gas_limit: gas_used,
            max_fee_per_gas: ctx.base_fee().into(),
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(Address::ZERO),
            // Include the message as part of the transaction data
            input: message.into(),
            ..Default::default()
        });
        // Sign the transaction
        let builder_tx = signer
            .sign_tx(tx)
            .map_err(BuilderTransactionError::SigningError)?;

        Ok(builder_tx)
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
            let flashblocks_builder_tx = self.simulate_builder_tx(ctx, db)?;
            builder_txs.extend(flashblocks_builder_tx.clone());
        }

        if ctx.is_last_flashblock() {
            let flashblocks_builder_tx = self.simulate_builder_tx(ctx, db)?;
            builder_txs.extend(flashblocks_builder_tx.clone());
            if let Some(flashtestations_builder_tx) = &self.flashtestations_builder_tx {
                // We only include flashtestations txs in the last flashblock

                let mut simulation_state = self.simulate_builder_txs_state::<FlashblocksExtraCtx>(
                    state_provider.clone(),
                    flashblocks_builder_tx.iter().collect(),
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
