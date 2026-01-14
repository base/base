use core::fmt::Debug;

use alloy_evm::Database;
use reth_revm::State;
use revm::DatabaseRef;

use crate::{
    builders::{
        BuilderTransactionCtx, BuilderTransactionError, BuilderTransactions,
        builder_tx::BuilderTxBase, context::OpPayloadBuilderCtx,
    },
    tx_signer::Signer,
};

// This will be the end of block transaction of a regular block
#[derive(Debug, Clone)]
pub(super) struct StandardBuilderTx {
    pub base_builder_tx: BuilderTxBase,
}

impl StandardBuilderTx {
    pub(super) const fn new(signer: Option<Signer>) -> Self {
        let base_builder_tx = BuilderTxBase::new(signer);
        Self { base_builder_tx }
    }
}

impl BuilderTransactions for StandardBuilderTx {
    fn simulate_builder_txs(
        &self,
        ctx: &OpPayloadBuilderCtx,
        db: &mut State<impl Database + DatabaseRef>,
    ) -> Result<Vec<BuilderTransactionCtx>, BuilderTransactionError> {
        let mut builder_txs = Vec::<BuilderTransactionCtx>::new();
        let standard_builder_tx = self.base_builder_tx.simulate_builder_tx(ctx, &mut *db)?;
        builder_txs.extend(standard_builder_tx);
        Ok(builder_txs)
    }
}
