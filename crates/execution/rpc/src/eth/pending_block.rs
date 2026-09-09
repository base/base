//! Loads Base pending block for a RPC response.

use alloy_eips::BlockNumberOrTag;
use reth_rpc_eth_types::{
    EthApiError, PendingBlock, block::BlockAndReceipts, builder::config::PendingBlockKind,
};
use reth_storage_api::{BlockReaderIdExt, StateProviderBox};

use crate::{BaseEthApi, BaseEthApiError};

impl BaseEthApi {
    #[inline]
    pub fn pending_block(&self) -> &tokio::sync::Mutex<Option<PendingBlock>> {
        self.inner.pending_block()
    }

    #[inline]
    pub fn pending_block_kind(&self) -> PendingBlockKind {
        self.inner.pending_block_kind()
    }

    /// Returns a [`StateProviderBox`] on a mem-pool built pending block overlaying latest.
    pub async fn local_pending_state(&self) -> Result<Option<StateProviderBox>, BaseEthApiError> {
        Ok(None)
    }

    /// Returns the locally built pending block
    pub async fn local_pending_block(&self) -> Result<Option<BlockAndReceipts>, BaseEthApiError> {
        // See: <https://github.com/ethereum-optimism/op-geth/blob/f2e69450c6eec9c35d56af91389a1c47737206ca/miner/worker.go#L367-L375>
        let latest = self
            .provider()
            .latest_header()?
            .ok_or(EthApiError::HeaderNotFound(BlockNumberOrTag::Latest.into()))?;

        let latest = self
            .cache()
            .get_block_and_receipts(latest.hash())
            .await
            .map_err(BaseEthApiError::from_eth_err)?
            .map(|(block, receipts)| BlockAndReceipts { block, receipts });
        Ok(latest)
    }
}
