use alloy_evm::Database;
use alloy_primitives::{
    Address,
    map::foldhash::{HashSet, HashSetExt},
};
use core::fmt::Debug;
use op_revm::OpTransactionError;
use reth_evm::{ConfigureEvm, Evm, eth::receipt_builder::ReceiptBuilderCtx};
use reth_node_api::PayloadBuilderError;
use reth_optimism_primitives::OpTransactionSigned;
use reth_primitives::Recovered;
use reth_provider::{ProviderError, StateProvider};
use reth_revm::{
    State, database::StateProviderDatabase, db::states::bundle_state::BundleRetention,
};
use revm::{
    DatabaseCommit,
    context::result::{EVMError, ResultAndState},
};
use tracing::warn;

use crate::{builders::context::OpPayloadBuilderCtx, primitives::reth::ExecutionInfo};

#[derive(Debug, Clone)]
pub struct BuilderTransactionCtx {
    pub gas_used: u64,
    pub da_size: u64,
    pub signed_tx: Recovered<OpTransactionSigned>,
}

/// Possible error variants during construction of builder txs.
#[derive(Debug, thiserror::Error)]
pub enum BuilderTransactionError {
    /// Builder account load fails to get builder nonce
    #[error("failed to load account {0}")]
    AccountLoadFailed(Address),
    /// Signature signing fails
    #[error("failed to sign transaction: {0}")]
    SigningError(secp256k1::Error),
    /// Unrecoverable error during evm execution.
    #[error("evm execution error {0}")]
    EvmExecutionError(Box<dyn core::error::Error + Send + Sync>),
    /// Any other builder transaction errors.
    #[error(transparent)]
    Other(Box<dyn core::error::Error + Send + Sync>),
}

impl From<secp256k1::Error> for BuilderTransactionError {
    fn from(error: secp256k1::Error) -> Self {
        BuilderTransactionError::SigningError(error)
    }
}

impl From<EVMError<ProviderError, OpTransactionError>> for BuilderTransactionError {
    fn from(error: EVMError<ProviderError, OpTransactionError>) -> Self {
        BuilderTransactionError::EvmExecutionError(Box::new(error))
    }
}

impl From<BuilderTransactionError> for PayloadBuilderError {
    fn from(error: BuilderTransactionError) -> Self {
        match error {
            BuilderTransactionError::EvmExecutionError(e) => {
                PayloadBuilderError::EvmExecutionError(e)
            }
            _ => PayloadBuilderError::Other(Box::new(error)),
        }
    }
}

pub trait BuilderTransactions<ExtraCtx: Debug + Default = ()>: Debug {
    fn simulate_builder_txs<Extra: Debug + Default>(
        &self,
        state_provider: impl StateProvider + Clone,
        info: &mut ExecutionInfo<Extra>,
        ctx: &OpPayloadBuilderCtx<ExtraCtx>,
        db: &mut State<impl Database>,
    ) -> Result<Vec<BuilderTransactionCtx>, BuilderTransactionError>;

    fn add_builder_txs<Extra: Debug + Default>(
        &self,
        state_provider: impl StateProvider + Clone,
        info: &mut ExecutionInfo<Extra>,
        builder_ctx: &OpPayloadBuilderCtx<ExtraCtx>,
        db: &mut State<impl Database>,
    ) -> Result<Vec<BuilderTransactionCtx>, BuilderTransactionError> {
        {
            let mut evm = builder_ctx
                .evm_config
                .evm_with_env(&mut *db, builder_ctx.evm_env.clone());

            let mut invalid: HashSet<Address> = HashSet::new();

            let builder_txs =
                self.simulate_builder_txs(state_provider, info, builder_ctx, evm.db_mut())?;
            for builder_tx in builder_txs.iter() {
                if invalid.contains(&builder_tx.signed_tx.signer()) {
                    warn!(target: "payload_builder", tx_hash = ?builder_tx.signed_tx.tx_hash(), "builder signer invalid as previous builder tx reverted");
                    continue;
                }

                let ResultAndState { result, state } = evm
                    .transact(&builder_tx.signed_tx)
                    .map_err(|err| BuilderTransactionError::EvmExecutionError(Box::new(err)))?;

                if !result.is_success() {
                    warn!(target: "payload_builder", tx_hash = ?builder_tx.signed_tx.tx_hash(), "builder tx reverted");
                    invalid.insert(builder_tx.signed_tx.signer());
                    continue;
                }

                // Add gas used by the transaction to cumulative gas used, before creating the receipt
                let gas_used = result.gas_used();
                info.cumulative_gas_used += gas_used;

                let ctx = ReceiptBuilderCtx {
                    tx: builder_tx.signed_tx.inner(),
                    evm: &evm,
                    result,
                    state: &state,
                    cumulative_gas_used: info.cumulative_gas_used,
                };
                info.receipts.push(builder_ctx.build_receipt(ctx, None));

                // Commit changes
                evm.db_mut().commit(state);

                // Append sender and transaction to the respective lists
                info.executed_senders.push(builder_tx.signed_tx.signer());
                info.executed_transactions
                    .push(builder_tx.signed_tx.clone().into_inner());
            }

            // Release the db reference by dropping evm
            drop(evm);

            Ok(builder_txs)
        }
    }

    fn simulate_builder_txs_state<Extra: Debug + Default>(
        &self,
        state_provider: impl StateProvider + Clone,
        builder_txs: Vec<&BuilderTransactionCtx>,
        ctx: &OpPayloadBuilderCtx<ExtraCtx>,
        db: &mut State<impl Database>,
    ) -> Result<State<StateProviderDatabase<impl StateProvider>>, BuilderTransactionError> {
        let state = StateProviderDatabase::new(state_provider.clone());
        let mut simulation_state = State::builder()
            .with_database(state)
            .with_bundle_prestate(db.bundle_state.clone())
            .with_bundle_update()
            .build();
        let mut evm = ctx
            .evm_config
            .evm_with_env(&mut simulation_state, ctx.evm_env.clone());

        for builder_tx in builder_txs {
            let ResultAndState { state, .. } = evm
                .transact(&builder_tx.signed_tx)
                .map_err(|err| BuilderTransactionError::EvmExecutionError(Box::new(err)))?;

            evm.db_mut().commit(state);
            evm.db_mut().merge_transitions(BundleRetention::Reverts);
        }

        Ok(simulation_state)
    }
}
