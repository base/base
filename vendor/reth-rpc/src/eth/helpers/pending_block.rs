//! Support for building a pending block with transactions from local view of mempool.

use reth_rpc_eth_api::{
    FromEvmError, RpcNodeCore,
    helpers::{LoadPendingBlock, pending_block::PendingEnvBuilder},
};
use reth_rpc_eth_types::{EthApiError, PendingBlock, builder::config::PendingBlockKind};

use crate::EthApi;

impl<N> LoadPendingBlock for EthApi<N>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError,
{
    #[inline]
    fn pending_block(&self) -> &tokio::sync::Mutex<Option<PendingBlock>> {
        self.inner.pending_block()
    }

    #[inline]
    fn pending_env_builder(&self) -> &dyn PendingEnvBuilder {
        self.inner.pending_env_builder()
    }

    #[inline]
    fn pending_block_kind(&self) -> PendingBlockKind {
        self.inner.pending_block_kind()
    }
}
