use std::fmt::Debug;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use async_trait::async_trait;
use base_common_client_ethereum::{Provider, RootProvider};
use base_consensus_batch::L2BlockInfo;
use base_consensus_source::LocalL2Provider;
use base_execution_state_tasks::ProofsProgress;

use crate::follow::error::FollowError;

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub(super) trait FollowLocalClient: Debug + Send + Sync {
    async fn block_info(&self, tag: BlockNumberOrTag) -> Result<Option<L2BlockInfo>, FollowError>;

    async fn l1_block_hash(&self, number: u64) -> Result<Option<B256>, FollowError>;

    async fn proofs_latest(&self) -> Result<Option<u64>, FollowError>;
}

#[derive(Clone, Debug)]
pub(super) struct LocalL2Client {
    provider: LocalL2Provider,
    l1_provider: RootProvider,
    proofs_progress: Option<ProofsProgress>,
}

impl LocalL2Client {
    pub(super) const fn new(
        provider: LocalL2Provider,
        l1_provider: RootProvider,
        proofs_progress: Option<ProofsProgress>,
    ) -> Self {
        Self { provider, l1_provider, proofs_progress }
    }
}

#[async_trait]
impl FollowLocalClient for LocalL2Client {
    async fn block_info(&self, tag: BlockNumberOrTag) -> Result<Option<L2BlockInfo>, FollowError> {
        self.provider
            .block_info(tag.into())
            .await
            .map_err(|source| FollowError::LocalBlockFetch { tag, source })
    }

    async fn l1_block_hash(&self, number: u64) -> Result<Option<B256>, FollowError> {
        self.l1_provider
            .get_header_by_number(number.into())
            .await
            .map(|header| header.map(|header| header.hash))
            .map_err(|source| FollowError::LocalL1BlockFetch { number, source })
    }

    async fn proofs_latest(&self) -> Result<Option<u64>, FollowError> {
        self.proofs_progress
            .as_ref()
            .ok_or(FollowError::ProofsUnavailable)?
            .latest()
            .await
            .map_err(FollowError::ProofsStatus)
    }
}
