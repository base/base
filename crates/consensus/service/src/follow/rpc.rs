use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use async_trait::async_trait;
use base_consensus_rpc::{HealthzApiServer, HealthzRpc, RpcBuilder, SyncStatusApiServer};
use base_protocol::{L2BlockInfo, SyncStatus};
use jsonrpsee::{
    RpcModule,
    core::RpcResult,
    server::ServerHandle,
    types::{ErrorCode, ErrorObject},
};
use tokio_util::sync::CancellationToken;

use crate::{
    NodeActor,
    actors::launch_rpc_server,
    follow::{error::FollowError, local::FollowLocalClient},
};

#[derive(Debug)]
struct FollowSyncStatusRpc<L> {
    local: Arc<L>,
}

impl<L> FollowSyncStatusRpc<L> {
    const fn new(local: Arc<L>) -> Self {
        Self { local }
    }

    async fn block_or_default(&self, tag: BlockNumberOrTag) -> RpcResult<L2BlockInfo>
    where
        L: FollowLocalClient,
    {
        self.local
            .block_info(tag)
            .await
            .map_err(|e| {
                ErrorObject::owned(ErrorCode::InternalError.code(), e.to_string(), None::<()>)
            })
            .map(|block| block.unwrap_or_default())
    }
}

#[async_trait]
impl<L> SyncStatusApiServer for FollowSyncStatusRpc<L>
where
    L: FollowLocalClient + 'static,
{
    async fn sync_status(&self) -> RpcResult<SyncStatus> {
        let unsafe_l2 = self.block_or_default(BlockNumberOrTag::Latest).await?;
        let safe_l2 = self.block_or_default(BlockNumberOrTag::Safe).await?;
        let finalized_l2 = self.block_or_default(BlockNumberOrTag::Finalized).await?;

        Ok(SyncStatus {
            unsafe_l2,
            local_safe_l2: safe_l2,
            safe_l2,
            finalized_l2,
            ..Default::default()
        })
    }
}

#[derive(Debug)]
pub(super) struct FollowRpcActor<L> {
    config: RpcBuilder,
    local: Arc<L>,
}

impl<L> FollowRpcActor<L> {
    pub(super) const fn new(config: RpcBuilder, local: Arc<L>) -> Self {
        Self { config, local }
    }

    async fn launch(&self, module: RpcModule<()>) -> Result<ServerHandle, FollowError> {
        launch_rpc_server(&self.config, module)
            .await
            .map_err(|e| FollowError::RpcServer(e.to_string()))
    }
}

#[async_trait]
impl<L> NodeActor for FollowRpcActor<L>
where
    L: FollowLocalClient + 'static,
{
    type Error = FollowError;
    type StartData = CancellationToken;

    async fn start(self, cancellation: Self::StartData) -> Result<(), Self::Error> {
        let mut modules = RpcModule::new(());
        modules
            .merge(HealthzApiServer::into_rpc(HealthzRpc {}))
            .map_err(|e| FollowError::RpcModule(e.to_string()))?;
        modules
            .merge(FollowSyncStatusRpc::new(Arc::clone(&self.local)).into_rpc())
            .map_err(|e| FollowError::RpcModule(e.to_string()))?;

        let restarts = self.config.restart_count();
        let mut handle = self.launch(modules.clone()).await?;

        for _ in 0..=restarts {
            tokio::select! {
                _ = handle.clone().stopped() => {
                    handle = self.launch(modules.clone()).await?;
                }
                _ = cancellation.cancelled() => {
                    handle.stop().map_err(|e| FollowError::RpcStop(format!("{e:?}")))?;
                    return Ok(());
                }
            }
        }

        cancellation.cancel();
        Err(FollowError::RpcRestartLimit)
    }
}
