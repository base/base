use std::sync::Arc;

use crate::{
    metrics::OpRBuilderMetrics,
    primitives::bundle::{Bundle, BundleResult},
    tx::{FBPooledTransaction, MaybeRevertingTransaction},
};
use alloy_json_rpc::RpcObject;
use alloy_primitives::B256;
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use moka::future::Cache;
use reth::rpc::api::eth::{helpers::FullEthApi, RpcReceipt};
use reth_optimism_txpool::{conditional::MaybeConditionalTransaction, OpPooledTransaction};
use reth_provider::StateProviderFactory;
use reth_rpc_eth_types::{utils::recover_raw_transaction, EthApiError};
use reth_transaction_pool::{PoolTransaction, TransactionOrigin, TransactionPool};

// We have to split the RPC modules in two sets because we have methods that both
// replace an existing method and add a new one.
// Tracking change in Reth here to have a single method for both:
// https://github.com/paradigmxyz/reth/issues/16502

// Namespace overrides for revert protection support
#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait EthApiExt {
    #[method(name = "sendBundle")]
    async fn send_bundle(&self, tx: Bundle) -> RpcResult<BundleResult>;
}

#[rpc(server, client, namespace = "eth")]
pub trait EthApiOverride<R: RpcObject> {
    #[method(name = "getTransactionReceipt")]
    async fn transaction_receipt(&self, hash: B256) -> RpcResult<Option<R>>;
}

pub struct RevertProtectionExt<Pool, Provider, Eth> {
    pool: Pool,
    provider: Provider,
    eth_api: Eth,
    metrics: Arc<OpRBuilderMetrics>,
}

impl<Pool, Provider, Eth> RevertProtectionExt<Pool, Provider, Eth>
where
    Pool: Clone,
    Provider: Clone,
    Eth: Clone,
{
    pub fn new(pool: Pool, provider: Provider, eth_api: Eth) -> Self {
        Self {
            pool,
            provider,
            eth_api,
            metrics: Arc::new(OpRBuilderMetrics::default()),
        }
    }

    pub fn bundle_api(&self) -> RevertProtectionBundleAPI<Pool, Provider> {
        RevertProtectionBundleAPI {
            pool: self.pool.clone(),
            provider: self.provider.clone(),
            metrics: self.metrics.clone(),
        }
    }

    pub fn eth_api(&self, reverted_cache: Cache<B256, ()>) -> RevertProtectionEthAPI<Eth> {
        RevertProtectionEthAPI {
            eth_api: self.eth_api.clone(),
            reverted_cache,
        }
    }
}

pub struct RevertProtectionBundleAPI<Pool, Provider> {
    pool: Pool,
    provider: Provider,
    metrics: Arc<OpRBuilderMetrics>,
}

#[async_trait]
impl<Pool, Provider> EthApiExtServer for RevertProtectionBundleAPI<Pool, Provider>
where
    Pool: TransactionPool<Transaction = FBPooledTransaction> + Clone + 'static,
    Provider: StateProviderFactory + Send + Sync + Clone + 'static,
{
    async fn send_bundle(&self, bundle: Bundle) -> RpcResult<BundleResult> {
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

        let conditional = bundle
            .conditional(last_block_number)
            .map_err(EthApiError::from)?;

        let recovered = recover_raw_transaction(&bundle_transaction)?;
        let mut pool_transaction: FBPooledTransaction =
            OpPooledTransaction::from_pooled(recovered).into();

        pool_transaction.set_reverted_hashes(bundle.reverting_hashes.clone().unwrap_or_default());
        pool_transaction.set_conditional(conditional);

        let hash = self
            .pool
            .add_transaction(TransactionOrigin::Local, pool_transaction)
            .await
            .map_err(EthApiError::from)?;

        self.metrics.bundles_received.increment(1);

        let result = BundleResult { bundle_hash: hash };
        Ok(result)
    }
}

pub struct RevertProtectionEthAPI<Eth> {
    eth_api: Eth,
    reverted_cache: Cache<B256, ()>,
}

#[async_trait]
impl<Eth> EthApiOverrideServer<RpcReceipt<Eth::NetworkTypes>> for RevertProtectionEthAPI<Eth>
where
    Eth: FullEthApi + Send + Sync + Clone + 'static,
{
    async fn transaction_receipt(
        &self,
        hash: B256,
    ) -> RpcResult<Option<RpcReceipt<Eth::NetworkTypes>>> {
        match self.eth_api.transaction_receipt(hash).await.unwrap() {
            Some(receipt) => Ok(Some(receipt)),
            None => {
                // Try to find the transaction in the reverted cache
                if self.reverted_cache.get(&hash).await.is_some() {
                    return Err(EthApiError::InvalidParams(
                        "the transaction was dropped from the pool".into(),
                    )
                    .into());
                } else {
                    return Ok(None);
                }
            }
        }
    }
}
