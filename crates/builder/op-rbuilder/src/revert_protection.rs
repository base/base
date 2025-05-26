use crate::{
    primitives::bundle::{Bundle, BundleResult, MAX_BLOCK_RANGE_BLOCKS},
    tx::{FBPooledTransaction, MaybeRevertingTransaction},
};
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use reth_optimism_txpool::{conditional::MaybeConditionalTransaction, OpPooledTransaction};
use reth_provider::StateProviderFactory;
use reth_rpc_eth_types::{utils::recover_raw_transaction, EthApiError};
use reth_transaction_pool::{PoolTransaction, TransactionOrigin, TransactionPool};

// Namespace overrides for revert protection support
#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait EthApiOverride {
    #[method(name = "sendBundle")]
    async fn send_bundle(&self, tx: Bundle) -> RpcResult<BundleResult>;
}

pub struct RevertProtectionExt<Pool, Provider> {
    pool: Pool,
    provider: Provider,
}

impl<Pool, Provider> RevertProtectionExt<Pool, Provider> {
    pub fn new(pool: Pool, provider: Provider) -> Self {
        Self { pool, provider }
    }
}

#[async_trait]
impl<Pool, Provider> EthApiOverrideServer for RevertProtectionExt<Pool, Provider>
where
    Pool: TransactionPool<Transaction = FBPooledTransaction> + Clone + 'static,
    Provider: StateProviderFactory + Send + Sync + Clone + 'static,
{
    async fn send_bundle(&self, mut bundle: Bundle) -> RpcResult<BundleResult> {
        let last_block_number = self
            .provider
            .best_block_number()
            .map_err(|_e| EthApiError::InternalEthError)?;

        // Only one transaction in the bundle is expected
        let bundle_transaction = match bundle.transactions.len() {
            0 => {
                return Err(EthApiError::InvalidParams(
                    "bundle must contain at least one transaction".into(),
                )
                .into());
            }
            1 => bundle.transactions[0].clone(),
            _ => {
                return Err(EthApiError::InvalidParams(
                    "bundle must contain exactly one transaction".into(),
                )
                .into());
            }
        };

        if let Some(block_number_max) = bundle.block_number_max {
            // The max block cannot be a past block
            if block_number_max <= last_block_number {
                return Err(
                    EthApiError::InvalidParams("block_number_max is a past block".into()).into(),
                );
            }

            // Validate that it is not greater than the max_block_range
            if block_number_max > last_block_number + MAX_BLOCK_RANGE_BLOCKS {
                return Err(
                    EthApiError::InvalidParams("block_number_max is too high".into()).into(),
                );
            }
        } else {
            // If no upper bound is set, use the maximum block range
            bundle.block_number_max = Some(last_block_number + MAX_BLOCK_RANGE_BLOCKS);
        }

        let recovered = recover_raw_transaction(&bundle_transaction)?;
        let mut pool_transaction: FBPooledTransaction =
            OpPooledTransaction::from_pooled(recovered).into();

        pool_transaction.set_exclude_reverting_txs(true);
        pool_transaction.set_conditional(bundle.conditional());

        let hash = self
            .pool
            .add_transaction(TransactionOrigin::Local, pool_transaction)
            .await
            .map_err(EthApiError::from)?;

        let result = BundleResult { bundle_hash: hash };
        Ok(result)
    }
}
