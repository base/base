//! The rollup node whose derivation the batcher follows.

use std::sync::Arc;

use base_batcher_core::DerivationStatus;
use base_consensus_rpc::RollupNodeApiClient;
use jsonrpsee::http_client::HttpClient;

use crate::{DerivationStatusProvider, RpcClientBuilder, Sequencers, ShadowConfig};

/// The rollup node whose rollup config the batcher reads and whose derivation it follows.
#[derive(Debug)]
pub enum RollupNode {
    /// The rollup node of the leader sequencer, which a canonical batcher follows.
    Leader(Arc<Sequencers>),
    /// The rollup node of the parity validator, which a shadow batcher follows.
    ParityValidator(HttpClient),
}

impl RollupNode {
    /// The rollup node of the parity validator of `shadow`, or of the leader among `sequencers`
    /// for a canonical batcher.
    ///
    /// # Errors
    ///
    /// Returns an error when the parity validator URL is not an HTTP URL.
    pub fn new(
        shadow: Option<&ShadowConfig>,
        sequencers: Arc<Sequencers>,
        client_builder: RpcClientBuilder,
    ) -> eyre::Result<Self> {
        match shadow {
            Some(shadow) => {
                Ok(Self::ParityValidator(client_builder.client(&shadow.validator_rollup_rpc)?))
            }
            None => Ok(Self::Leader(sequencers)),
        }
    }

    /// The client of the rollup node: for [`Leader`](Self::Leader), the client of the current
    /// leader.
    pub fn client(&self) -> &HttpClient {
        match self {
            Self::Leader(sequencers) => &sequencers.leader().rollup_node_client,
            Self::ParityValidator(client) => client,
        }
    }
}

/// Reads the derivation status from the rollup node's `optimism_syncStatus`.
impl DerivationStatusProvider for RollupNode {
    async fn derivation_status(
        &self,
    ) -> Result<DerivationStatus, Box<dyn std::error::Error + Send + Sync>> {
        let status = self.client().sync_status().await?;
        Ok(DerivationStatus {
            safe_l2: status.local_safe_l2.block_info,
            current_l1: status.current_l1,
        })
    }
}

#[cfg(test)]
mod tests {
    use jsonrpsee::{core::client::ClientT, rpc_params};

    use super::*;
    use crate::test_utils::{Activity, FakeSequencer, rpc_client_builder};

    /// A canonical batcher's rollup node is the one of the current leader, so a request made
    /// after a leader change reaches the new leader.
    #[tokio::test]
    async fn leader_rollup_node_is_the_current_leader() {
        let first = FakeSequencer::start(Activity::Active, 1).await;
        let second = FakeSequencer::start(Activity::NotLeader, 2).await;
        let sequencers = Arc::new(
            Sequencers::new(&[first.url.clone(), second.url.clone()], rpc_client_builder())
                .unwrap(),
        );
        sequencers.refresh_leader().await.unwrap();
        let rollup_node =
            RollupNode::new(None, Arc::clone(&sequencers), rpc_client_builder()).unwrap();

        first.set_activity(Activity::NotLeader);
        second.set_activity(Activity::Active);
        sequencers.refresh_leader().await.unwrap();

        let chain_id: String =
            rollup_node.client().request("eth_chainId", rpc_params![]).await.unwrap();
        assert_eq!(chain_id, "0x2");
    }
}
