//! L1-free development chain over an execution node's snapshot datadir.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_eips::BlockNumberOrTag;
use alloy_rpc_types_engine::JwtSecret;
use base_common_chains::BaseUpgradeExt;
use base_common_consensus::REGOLITH_SYSTEM_TX_GAS;
use base_common_genesis::{BaseFeature, BaseUpgrade, RollupConfig};
use base_consensus_engine::{EngineClient, EngineClientError, EngineEndpoint};
use base_protocol::{BaseBlockConversionError, BaseTimeUpdateTx, L2BlockMetadata};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

use crate::{
    EngineConfig, NodeOperatingMode, ShutdownSignal, StandaloneImpersonationQueue,
    StandaloneSequencerNode,
};

/// L1-free development chain extending the head of an execution node's snapshot datadir.
///
/// The head must already be on the chain's Denim schedule, so every run, including a restart,
/// extends it with the canonical rollup configuration and the head's gas parameters.
#[derive(Debug)]
pub struct StandaloneDevChain {
    /// Canonical rollup configuration of the datadir's chain.
    pub rollup_config: RollupConfig,
}

impl StandaloneDevChain {
    /// Creates a development chain for the datadir's canonical rollup configuration.
    pub const fn new(rollup_config: RollupConfig) -> Self {
        Self { rollup_config }
    }

    /// Validates the head against the chain's Denim schedule and sequences until `cancellation`
    /// is triggered.
    ///
    /// `impersonation` starts accepting requests once the head is validated, and its deposits are
    /// sequenced after the system deposits of each block.
    ///
    /// Returns an error if the sequencer stops for any other reason.
    pub async fn run(
        self,
        engine_endpoint: EngineEndpoint,
        jwt_secret: JwtSecret,
        impersonation: StandaloneImpersonationQueue,
        cancellation: CancellationToken,
    ) -> Result<(), StandaloneDevError> {
        let rollup_config = self.rollup_config;
        let engine = EngineConfig {
            config: Arc::new(rollup_config.clone()),
            l2_endpoint: engine_endpoint,
            l2_jwt_secret: jwt_secret,
            // Standalone sequencing never issues an L1 request.
            l1_url: Url::parse("http://127.0.0.1:1").expect("valid unused L1 URL"),
            l1_rpc_timeout: base_consensus_providers::L1_RPC_TIMEOUT,
            mode: NodeOperatingMode::Sequencer,
        }
        .build_engine_client()
        .await
        .map_err(EngineClientError::from)?;

        let head = engine
            .l2_block_by_label(BlockNumberOrTag::Latest)
            .await?
            .ok_or(StandaloneDevError::MissingBlock(BlockNumberOrTag::Latest))?
            .map_header(|header| header.into_inner())
            .into_consensus()
            .map_transactions(|transaction| transaction.inner.inner.into_inner());
        let head_number = head.header.number;
        if head_number == 0 {
            return Err(StandaloneDevError::GenesisHead);
        }
        if !rollup_config
            .is_feature_active_cascading(BaseFeature::SubsecondBlocks, head.header.timestamp)
        {
            return Err(StandaloneDevError::PreDenimHead(head_number));
        }
        let head_millis = BaseTimeUpdateTx::extract_timestamp_ms(
            &head.body.transactions,
            head_number,
            head.header.timestamp,
        )
        .ok();
        let scheduled_millis = rollup_config.l2_block_timestamp_millis(head_number);
        if head_millis != Some(scheduled_millis) {
            return Err(StandaloneDevError::OffScheduleHead(head_number));
        }
        let metadata = L2BlockMetadata::from_block(&head, &rollup_config)?;

        let head_time = UNIX_EPOCH + Duration::from_millis(scheduled_millis);
        let schedule_delay = SystemTime::now().duration_since(head_time).unwrap_or_default();
        info!(
            target: "standalone_dev",
            head = head_number,
            head_hash = %head.header.hash_slow(),
            "starting development chain"
        );
        warn!(
            target: "standalone_dev",
            behind_wall_clock_secs = schedule_delay.as_secs(),
            "block timestamps continue from the snapshot, not wall-clock time"
        );

        // Every development block also carries the L1-info and BaseTime system deposits.
        impersonation.activate(
            metadata.system_config.gas_limit.saturating_sub(2 * REGOLITH_SYSTEM_TX_GAS),
            BaseUpgrade::from_timestamp(
                &rollup_config,
                rollup_config.l2_block_timestamp(head_number + 1),
            ),
        );
        let mut node = StandaloneSequencerNode::new(
            Arc::new(rollup_config),
            Arc::new(engine),
            metadata.l1_info,
            metadata.system_config,
            None,
        );
        node.schedule_delay = schedule_delay;
        node.attributes_builder.impersonation = Some(impersonation);
        // The sequencer also stops cleanly on a shutdown signal or when any actor exits. Polling the
        // signal first, and giving actors a child token, distinguishes requested shutdowns from an
        // unexpected stop.
        let result = tokio::select! {
            biased;
            () = ShutdownSignal::wait() => return Ok(()),
            result = node.start_with_cancellation(cancellation.child_token()) => result,
        };
        if cancellation.is_cancelled() {
            return Ok(());
        }
        result.map_err(StandaloneDevError::Sequencer)?;
        Err(StandaloneDevError::Stopped)
    }
}

/// Error starting or running a [`StandaloneDevChain`].
#[derive(Debug, thiserror::Error)]
pub enum StandaloneDevError {
    /// The execution engine request failed.
    #[error("execution engine request failed: {0}")]
    Engine(#[from] EngineClientError),
    /// The execution node does not have a required block.
    #[error("execution node has no {0} block")]
    MissingBlock(BlockNumberOrTag),
    /// The datadir contains only the genesis block.
    #[error("datadir contains only genesis; use a snapshot datadir")]
    GenesisHead,
    /// The head predates the chain's Denim activation.
    #[error("head {0} predates Denim; download a current snapshot")]
    PreDenimHead(u64),
    /// The head is not on the chain's Denim schedule.
    #[error(
        "head {0} is not on this chain's Denim schedule; datadirs from an older `base dev` that \
         activated Denim locally cannot be resumed, download a current snapshot"
    )]
    OffScheduleHead(u64),
    /// The head lacks the metadata needed to extend it.
    #[error("cannot extend snapshot head: {0}")]
    Head(#[from] BaseBlockConversionError),
    /// The sequencer failed.
    #[error("standalone sequencer failed: {0}")]
    Sequencer(String),
    /// The sequencer stopped without a shutdown request.
    #[error("standalone sequencer stopped unexpectedly")]
    Stopped,
}
