//! Loads a pending block from database. Helper trait for `eth_` block, transaction, call and trace
//! RPC methods.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_eips::eip7840::BlobParams;
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::{B256, U256};
use base_common_chain_config::ChainSpecProvider;
use base_common_types_chain::{BlockHeader, InvalidTransactionError, SealedHeader, Transaction};
use base_common_types_rpc::BlockNumberOrTag;
use base_execution_evm_blocks::{
    BaseNextBlockEnvAttributes, BlockBuilderOutcome, BlockExecutionOutput, Evm,
};
use base_execution_evm_runtime::{
    Block, BlockExecutionError, BlockValidationError, Cfg as _, State,
};
use base_execution_state_database::NoopProvider;
use base_execution_state_types::{
    BlockReader, BlockReaderIdExt, ComputedTrieData, ExecutedBlock, ProviderError,
    StateProviderFactory,
};
use base_execution_txpool::{
    BestTransactions, BestTransactionsAttributes, InvalidPoolTransactionError, TransactionPool,
};
use futures::Future;
use tracing::debug;

use crate::{
    BaseEthApi, BaseEthApiError, EthApiError, PendingBlock, PendingBlockEnv, PendingBlockEnvOrigin,
};

/// Loads a pending block from database.
///
/// Behaviour shared by several `eth_` RPC methods, not exclusive to `eth_` blocks RPC methods.
impl BaseEthApi {
    /// Configures the [`PendingBlockEnv`] for the pending block
    ///
    /// If no pending block is available, this will derive it from the `latest` block
    pub fn pending_block_env_and_cfg(&self) -> Result<PendingBlockEnv, BaseEthApiError> {
        if let Some((block, receipts)) =
            self.provider().pending_block_and_receipts().map_err(BaseEthApiError::from_eth_err)?
        {
            // Note: for the PENDING block we assume it is past the known merge block and
            // thus this will not fail when looking up the total
            // difficulty value for the blockenv.
            let evm_env = self
                .evm_config()
                .evm_env(block.header())
                .map_err(|error| crate::EthApiError::Internal(error.into()))
                .map_err(BaseEthApiError::from_eth_err)?;

            return Ok(PendingBlockEnv::new(
                evm_env,
                PendingBlockEnvOrigin::ActualPending(Arc::new(block), Arc::new(receipts)),
            ));
        }

        // no pending block from the CL yet, so we use the latest block and modify the env
        // values that we can
        let latest = self
            .provider()
            .latest_header()
            .map_err(BaseEthApiError::from_eth_err)?
            .ok_or(EthApiError::HeaderNotFound(BlockNumberOrTag::Latest.into()))?;

        let evm_env = self
            .evm_config()
            .next_evm_env(&latest, &crate::BasePendingEnv::attributes(&latest))
            .map_err(|error| crate::EthApiError::Internal(error.into()))
            .map_err(BaseEthApiError::from_eth_err)?;

        Ok(PendingBlockEnv::new(evm_env, PendingBlockEnvOrigin::DerivedFromLatest(latest)))
    }

    /// Returns a mem-pool built pending block.
    pub fn pool_pending_block(
        &self,
    ) -> impl Future<Output = Result<Option<PendingBlock>, BaseEthApiError>> + Send {
        async move {
            if self.pending_block_kind().is_none() {
                return Ok(None);
            }
            let pending = self.pending_block_env_and_cfg()?;
            let parent = match pending.origin {
                PendingBlockEnvOrigin::ActualPending(..) => return Ok(None),
                PendingBlockEnvOrigin::DerivedFromLatest(parent) => parent,
            };

            self.build_pool_pending_block(parent, pending.evm_env).await
        }
    }

    /// Builds or returns a cached pending block from the transaction pool.
    ///
    /// This is the shared implementation used by both [`Self::pool_pending_block`] and
    /// [`Self::local_pending_block`] to avoid resolving the pending block environment twice.
    pub fn build_pool_pending_block(
        &self,
        parent: SealedHeader,
        evm_env: base_execution_evm_runtime::EvmEnv,
    ) -> impl Future<Output = Result<Option<PendingBlock>, BaseEthApiError>> + Send {
        async move {
            // we couldn't find the real pending block, so we need to build it ourselves
            let mut lock = self.pending_block().lock().await;

            let now = Instant::now();

            // Is the pending block cached?
            if let Some(pending_block) = lock.as_ref() {
                // Is the cached block not expired and latest is its parent?
                if evm_env.block_env.number() == U256::from(pending_block.block().number())
                    && parent.hash() == pending_block.block().parent_hash()
                    && now <= pending_block.expires_at
                {
                    return Ok(Some(pending_block.clone()));
                }
            }

            let executed_block = match self
                .spawn_blocking_io(move |this| {
                    // we rebuild the block
                    this.build_block(&parent)
                })
                .await
            {
                Ok(block) => block,
                Err(err) => {
                    debug!(target: "rpc", "Failed to build pending block: {:?}", err);
                    return Ok(None);
                }
            };

            let pending = PendingBlock::with_executed_block(
                Instant::now() + Duration::from_secs(1),
                executed_block,
            );

            *lock = Some(pending.clone());

            Ok(Some(pending))
        }
    }

    /// Builds a locally derived pending block using the configured provider and pool.
    ///
    /// This is used when no execution-layer pending block is available and a pending block is
    /// derived from the latest canonical header, using the provided parent.
    ///
    /// Withdrawals and any fork-specific behavior (such as EIP-4788 pre-block contract calls) are
    /// determined by the EVM environment and chain specification used during construction.
    pub fn build_block(&self, parent: &SealedHeader) -> Result<ExecutedBlock, BaseEthApiError>
    where
        EthApiError: From<ProviderError>,
    {
        let state_provider = self
            .provider()
            .history_by_block_hash(parent.hash())
            .map_err(BaseEthApiError::from_eth_err)?;
        let state = state_provider;
        let mut db = State::builder().with_database(state).with_bundle_update().build();

        let mut builder = self
            .evm_config()
            .builder_for_next_block(&mut db, parent, crate::BasePendingEnv::attributes(parent))
            .map_err(|error| crate::EthApiError::Internal(error.into()))
            .map_err(BaseEthApiError::from_eth_err)?;

        builder.apply_pre_execution_changes().map_err(BaseEthApiError::from_eth_err)?;

        let block_gas_limit: u64 = builder.evm().block().gas_limit();
        let is_amsterdam = self
            .provider()
            .chain_spec()
            .is_amsterdam_active_at_timestamp(builder.evm().block().timestamp().saturating_to());
        let basefee = builder.evm().block().basefee();
        let blob_gasprice = builder.evm().block().blob_gasprice().map(|p| p as u64);

        let blob_params = self
            .provider()
            .chain_spec()
            .blob_params_at_timestamp(parent.timestamp())
            .unwrap_or_else(BlobParams::cancun);
        let mut cumulative_tx_gas_used = 0;
        let mut block_regular_gas_used = 0;
        let mut block_state_gas_used = 0;
        let mut sum_blob_gas_used = 0;
        let tx_gas_limit_cap = builder.evm().cfg_env().tx_gas_limit_cap();

        // Only include transactions if not configured as Empty
        if !self.pending_block_kind().is_empty() {
            let mut best_txs = self
                .pool()
                .best_transactions_with_attributes(BestTransactionsAttributes::new(
                    basefee,
                    blob_gasprice,
                ))
                // freeze to get a block as fast as possible
                .without_updates();

            while let Some(pool_tx) = best_txs.next() {
                // ensure we still have capacity for this transaction
                let exceeds_gas_limit = if is_amsterdam {
                    let regular_available_gas =
                        block_gas_limit.saturating_sub(block_regular_gas_used);
                    let state_available_gas = block_gas_limit.saturating_sub(block_state_gas_used);
                    let regular_tx_gas_limit = pool_tx.gas_limit().min(tx_gas_limit_cap);

                    if regular_tx_gas_limit > regular_available_gas {
                        Some((regular_tx_gas_limit, regular_available_gas))
                    } else if pool_tx.gas_limit() > state_available_gas {
                        Some((pool_tx.gas_limit(), state_available_gas))
                    } else {
                        None
                    }
                } else {
                    let block_available_gas =
                        block_gas_limit.saturating_sub(cumulative_tx_gas_used);
                    (pool_tx.gas_limit() > block_available_gas)
                        .then_some((pool_tx.gas_limit(), block_available_gas))
                };

                if let Some((transaction_gas_limit, block_available_gas)) = exceeds_gas_limit {
                    // we can't fit this transaction into the block, so we need to mark it as
                    // invalid which also removes all dependent transaction from
                    // the iterator before we can continue
                    best_txs.mark_invalid(
                        &pool_tx,
                        InvalidPoolTransactionError::ExceedsGasLimit(
                            transaction_gas_limit,
                            block_available_gas,
                        ),
                    );
                    continue;
                }

                if pool_tx.origin.is_private() {
                    // we don't want to leak any state changes made by private transactions, so we
                    // mark them as invalid here which removes all dependent
                    // transactions from the iteratorbefore we can continue
                    best_txs.mark_invalid(
                        &pool_tx,
                        InvalidPoolTransactionError::Consensus(
                            InvalidTransactionError::TxTypeNotSupported,
                        ),
                    );
                    continue;
                }

                // convert tx to a signed transaction
                let tx = pool_tx.to_consensus();

                // There's only limited amount of blob space available per block, so we need to
                // check if the EIP-4844 can still fit in the block
                let tx_blob_gas = tx.blob_gas_used();
                if let Some(tx_blob_gas) = tx_blob_gas
                    && sum_blob_gas_used + tx_blob_gas > blob_params.max_blob_gas_per_block()
                {
                    // we can't fit this _blob_ transaction into the block, so we mark it as
                    // invalid, which removes its dependent transactions from
                    // the iterator. This is similar to the gas limit condition
                    // for regular transactions above.
                    best_txs.mark_invalid(
                        &pool_tx,
                        InvalidPoolTransactionError::ExceedsGasLimit(
                            tx_blob_gas,
                            blob_params.max_blob_gas_per_block(),
                        ),
                    );
                    continue;
                }

                let mut tx_regular_gas_used = 0;
                let gas_output =
                    match builder.execute_transaction_with_result_closure(tx, |result| {
                        tx_regular_gas_used = result.result.result.gas().block_regular_gas_used();
                    }) {
                        Ok(gas_output) => gas_output,
                        Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                            error,
                            ..
                        })) => {
                            if error.is_nonce_too_low() {
                                // if the nonce is too low, we can skip this transaction
                            } else {
                                // if the transaction is invalid, we can skip it and all of its
                                // descendants
                                best_txs.mark_invalid(
                                    &pool_tx,
                                    InvalidPoolTransactionError::Consensus(
                                        InvalidTransactionError::TxTypeNotSupported,
                                    ),
                                );
                            }
                            continue;
                        }
                        Err(BlockExecutionError::Validation(
                            BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                                transaction_gas_limit,
                                block_available_gas,
                            },
                        )) => {
                            best_txs.mark_invalid(
                                &pool_tx,
                                InvalidPoolTransactionError::ExceedsGasLimit(
                                    transaction_gas_limit,
                                    block_available_gas,
                                ),
                            );
                            continue;
                        }
                        // this is an error that we should treat as fatal for this attempt
                        Err(err) => return Err(BaseEthApiError::from_eth_err(err)),
                    };

                // add to the total blob gas used if the transaction successfully executed
                if let Some(tx_blob_gas) = tx_blob_gas {
                    sum_blob_gas_used += tx_blob_gas;

                    // if we've reached the max data gas per block, we can skip blob txs entirely
                    if sum_blob_gas_used == blob_params.max_blob_gas_per_block() {
                        best_txs.skip_blobs();
                    }
                }

                // Track receipt gas and the Amsterdam block-capacity counter separately.
                let gas_used = gas_output.tx_gas_used();
                cumulative_tx_gas_used += gas_used;
                block_regular_gas_used += tx_regular_gas_used;
                block_state_gas_used += gas_output.state_gas_used();
            }
        }

        let BlockBuilderOutcome { execution_result, block, hashed_state, trie_updates, .. } =
            builder.finish(NoopProvider::default(), None).map_err(BaseEthApiError::from_eth_err)?;

        let execution_outcome =
            BlockExecutionOutput { state: db.take_bundle(), result: execution_result };

        Ok(ExecutedBlock::new(
            block.into(),
            Arc::new(execution_outcome),
            ComputedTrieData::new(
                Arc::new(hashed_state.into_sorted()),
                Arc::new(trie_updates.into_sorted()),
            ),
        ))
    }
}

/// Constructs the pending block environment used by Base RPC handlers.
#[derive(Debug)]
pub struct BasePendingEnv;

impl BasePendingEnv {
    /// Derives pending block attributes from the parent header.
    pub fn attributes(parent: &SealedHeader) -> BaseNextBlockEnvAttributes {
        BaseNextBlockEnvAttributes {
            timestamp: parent.timestamp().saturating_add(12),
            suggested_fee_recipient: parent.beneficiary(),
            prev_randao: B256::random(),
            gas_limit: parent.gas_limit(),
            parent_beacon_block_root: parent.parent_beacon_block_root(),
            extra_data: parent.extra_data().clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_env_preserves_base_parent_beacon_root() {
        let beacon_root = B256::repeat_byte(0x42);
        let header = base_common_types_chain::Header {
            parent_beacon_block_root: Some(beacon_root),
            timestamp: 100,
            gas_limit: 30_000_000,
            ..Default::default()
        };
        let parent = SealedHeader::new(header, B256::ZERO);
        let attributes = BasePendingEnv::attributes(&parent);
        assert_eq!(attributes.parent_beacon_block_root, Some(beacon_root));
        assert_eq!(attributes.timestamp, 112);
        assert_eq!(attributes.gas_limit, 30_000_000);
    }
}
