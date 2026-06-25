use std::{fmt::Debug, sync::Arc};

use alloy_eips::BlockNumberOrTag;
use alloy_provider::{Provider, RootProvider};
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_protocol::L2BlockInfo;
use serde::Deserialize;

use crate::follow::error::FollowError;

#[derive(Debug, Deserialize)]
struct ProofsSyncStatus {
    latest: Option<u64>,
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub(super) trait FollowLocalClient: Debug + Send + Sync {
    async fn block_info(&self, tag: BlockNumberOrTag) -> Result<Option<L2BlockInfo>, FollowError>;

    async fn proofs_latest(&self) -> Result<Option<u64>, FollowError>;
}

#[derive(Clone, Debug)]
pub(super) struct LocalL2Client {
    provider: RootProvider<Base>,
    rollup_config: Arc<RollupConfig>,
}

impl LocalL2Client {
    pub(super) const fn new(
        provider: RootProvider<Base>,
        rollup_config: Arc<RollupConfig>,
    ) -> Self {
        Self { provider, rollup_config }
    }
}

#[async_trait]
impl FollowLocalClient for LocalL2Client {
    async fn block_info(&self, tag: BlockNumberOrTag) -> Result<Option<L2BlockInfo>, FollowError> {
        let block = self
            .provider
            .get_block_by_number(tag)
            .full()
            .await
            .map_err(|source| FollowError::LocalBlockFetch { tag, source })?;
        let Some(block) = block else {
            return Ok(None);
        };
        L2BlockInfo::from_block_and_genesis(
            &block.into_consensus().map_transactions(|tx| tx.inner.inner.into_inner()),
            &self.rollup_config.genesis,
        )
        .map(Some)
        .map_err(FollowError::from)
    }

    async fn proofs_latest(&self) -> Result<Option<u64>, FollowError> {
        self.provider
            .raw_request::<_, ProofsSyncStatus>("debug_proofsSyncStatus".into(), ())
            .await
            .map(|status| status.latest)
            .map_err(FollowError::ProofsStatus)
    }
}
