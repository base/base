use alloy_evm::Database;
use core::fmt::Debug;
use reth_provider::StateProvider;
use reth_revm::State;

use crate::{
    builders::{
        BuilderTransactionCtx, BuilderTransactionError, BuilderTransactions,
        builder_tx::BuilderTxBase, context::OpPayloadBuilderCtx,
    },
    flashtestations::builder_tx::FlashtestationsBuilderTx,
    primitives::reth::ExecutionInfo,
    tx_signer::Signer,
};

// This will be the end of block transaction of a regular block
#[derive(Debug, Clone)]
pub(super) struct StandardBuilderTx {
    pub base_builder_tx: BuilderTxBase,
    pub flashtestations_builder_tx: Option<FlashtestationsBuilderTx>,
}

impl StandardBuilderTx {
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

impl BuilderTransactions for StandardBuilderTx {
    fn simulate_builder_txs<Extra: Debug + Default>(
        &self,
        state_provider: impl StateProvider + Clone,
        info: &mut ExecutionInfo<Extra>,
        ctx: &OpPayloadBuilderCtx,
        db: &mut State<impl Database>,
    ) -> Result<Vec<BuilderTransactionCtx>, BuilderTransactionError> {
        let mut builder_txs = Vec::<BuilderTransactionCtx>::new();
        let standard_builder_tx = self.base_builder_tx.simulate_builder_tx(ctx, db)?;
        builder_txs.extend(standard_builder_tx.clone());
        if let Some(flashtestations_builder_tx) = &self.flashtestations_builder_tx {
            let mut simulation_state = self.simulate_builder_txs_state::<()>(
                state_provider.clone(),
                standard_builder_tx.iter().collect(),
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
        Ok(builder_txs)
    }
}
