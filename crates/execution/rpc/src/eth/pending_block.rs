//! Loads Base pending block for a RPC response.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_rpc_types_eth::BlockOverrides;
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseNextBlockEnvAttributes;
use reth_evm::ConfigureEvm;
use reth_primitives_traits::{NodePrimitives, SealedHeader};
use reth_rpc_eth_api::{
    FromEvmError, RpcConvert, RpcNodeCore, RpcNodeCoreExt,
    helpers::{
        LoadPendingBlock, SpawnBlocking,
        pending_block::{BuildPendingEnv, PendingEnvBuilder},
    },
};
use reth_rpc_eth_types::{
    EthApiError, PendingBlock, block::BlockAndReceipts, builder::config::PendingBlockKind,
    error::FromEthApiError,
};
use reth_storage_api::{BlockReaderIdExt, StateProviderBox};

use crate::{BaseEthApi, BaseEthApiError};

/// Builds Base pending-block attributes using the chain's legacy or Denim cadence.
#[derive(Debug, Clone)]
pub struct BasePendingEnvBuilder {
    chain_spec: Arc<BaseChainSpec>,
}

impl BasePendingEnvBuilder {
    /// Creates a pending environment builder for a Base chain specification.
    pub const fn new(chain_spec: Arc<BaseChainSpec>) -> Self {
        Self { chain_spec }
    }
}

impl<Evm> PendingEnvBuilder<Evm> for BasePendingEnvBuilder
where
    Evm: ConfigureEvm<NextBlockEnvCtx = BaseNextBlockEnvAttributes>,
{
    fn pending_env_attributes(
        &self,
        parent: &SealedHeader<<Evm::Primitives as NodePrimitives>::BlockHeader>,
        block_overrides: Option<&BlockOverrides>,
    ) -> Result<Evm::NextBlockEnvCtx, EthApiError> {
        let mut attributes = BaseNextBlockEnvAttributes::build_pending_env(parent, block_overrides);
        if block_overrides.and_then(|overrides| overrides.time).is_none() {
            attributes.timestamp =
                BaseNextBlockEnvAttributes::pending_timestamp(parent, self.chain_spec.as_ref());
        }
        Ok(attributes)
    }
}

impl<N, Rpc> LoadPendingBlock for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore,
    BaseEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = BaseEthApiError>,
{
    #[inline]
    fn pending_block(&self) -> &tokio::sync::Mutex<Option<PendingBlock<N::Primitives>>> {
        self.inner.eth_api.pending_block()
    }

    #[inline]
    fn pending_env_builder(&self) -> &dyn PendingEnvBuilder<Self::Evm> {
        self.inner.eth_api.pending_env_builder()
    }

    #[inline]
    fn pending_block_kind(&self) -> PendingBlockKind {
        self.inner.eth_api.pending_block_kind()
    }

    /// Returns a [`StateProviderBox`] on a mem-pool built pending block overlaying latest.
    async fn local_pending_state(&self) -> Result<Option<StateProviderBox>, Self::Error>
    where
        Self: SpawnBlocking,
    {
        Ok(None)
    }

    /// Returns the locally built pending block
    async fn local_pending_block(
        &self,
    ) -> Result<Option<BlockAndReceipts<Self::Primitives>>, Self::Error> {
        // See: <https://github.com/ethereum-optimism/op-geth/blob/f2e69450c6eec9c35d56af91389a1c47737206ca/miner/worker.go#L367-L375>
        let latest = self
            .provider()
            .latest_header()?
            .ok_or(EthApiError::HeaderNotFound(BlockNumberOrTag::Latest.into()))?;

        let latest = self
            .cache()
            .get_block_and_receipts(latest.hash())
            .await
            .map_err(Self::Error::from_eth_err)?
            .map(|(block, receipts)| BlockAndReceipts { block, receipts });
        Ok(latest)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::Header;
    use alloy_genesis::Genesis;
    use alloy_rpc_types_eth::BlockOverrides;
    use base_common_chains::BaseUpgrade;
    use base_common_consensus::BasePrimitives;
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use base_execution_evm::BaseEvmConfig;
    use reth_chainspec::ForkCondition;
    use reth_primitives_traits::SealedHeader;
    use reth_rpc_eth_api::helpers::pending_block::PendingEnvBuilder;

    use super::BasePendingEnvBuilder;

    #[test]
    fn pending_env_builder_uses_legacy_and_denim_cadences() {
        let chain_spec = BaseChainSpecBuilder::default()
            .chain(0.into())
            .genesis(Genesis::default())
            .with_fork(BaseUpgrade::Denim, ForkCondition::Timestamp(10))
            .build();
        let builder = BasePendingEnvBuilder::new(Arc::new(chain_spec));

        let parent =
            SealedHeader::seal_slow(Header { number: 4, timestamp: 8, ..Default::default() });
        let attributes = <BasePendingEnvBuilder as PendingEnvBuilder<
            BaseEvmConfig<BaseChainSpec, BasePrimitives>,
        >>::pending_env_attributes(&builder, &parent, None)
        .unwrap();
        assert_eq!(attributes.timestamp, 10);

        let parent =
            SealedHeader::seal_slow(Header { number: 0, timestamp: 0, ..Default::default() });
        let attributes = <BasePendingEnvBuilder as PendingEnvBuilder<
            BaseEvmConfig<BaseChainSpec, BasePrimitives>,
        >>::pending_env_attributes(&builder, &parent, None)
        .unwrap();
        assert_eq!(attributes.timestamp, 2);

        let overrides = BlockOverrides { time: Some(777), ..Default::default() };
        let attributes = <BasePendingEnvBuilder as PendingEnvBuilder<
            BaseEvmConfig<BaseChainSpec, BasePrimitives>,
        >>::pending_env_attributes(&builder, &parent, Some(&overrides))
        .unwrap();
        assert_eq!(attributes.timestamp, 777);
    }
}
