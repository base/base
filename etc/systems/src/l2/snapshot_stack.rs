//! L1-free snapshot-backed L2 stack for development and benchmarking.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_engine::JwtSecret;
use base_common_chains::ChainConfig;
use base_common_genesis::{BaseUpgrade, RollupConfig};
use base_common_network::Base;
use base_consensus_node::StandalonePrefund;
use base_execution_chainspec::BaseChainSpec;
use eyre::{Result, WrapErr, ensure};
use reth_ethereum_forks::ForkCondition;
use url::Url;

use super::{
    ChainSpecSource, InProcessBuilder, InProcessBuilderConfig, InProcessClient,
    InProcessClientConfig, InProcessFollowConsensus, InProcessFollowConsensusConfig,
    InProcessStandaloneSequencer, InProcessStandaloneSequencerConfig, L2ContainerConfig,
    SnapshotBoundary,
};
use crate::{DevnetBlockInterval, DevnetSnapshotConfig};

const SNAPSHOT_STARTUP_LEAD: Duration = Duration::from_secs(10);

/// Configuration for an L1-free snapshot-backed L2 stack.
#[derive(Debug, Clone)]
pub struct SnapshotL2StackConfig {
    /// Caller-owned writable snapshot datadirs and expected identity.
    pub snapshot: DevnetSnapshotConfig,
    /// Optional stable port assignments.
    pub container_config: Option<L2ContainerConfig>,
}

/// A running builder/standalone-sequencer/follow-client snapshot stack.
#[derive(Debug)]
pub struct SnapshotL2Stack {
    builder: InProcessBuilder,
    boundary: SnapshotBoundary,
    standalone_consensus: Option<InProcessStandaloneSequencer>,
    client: InProcessClient,
    follow_consensus: Option<InProcessFollowConsensus>,
    follow_config: Option<InProcessFollowConsensusConfig>,
    block_interval: DevnetBlockInterval,
}

impl SnapshotL2Stack {
    /// Starts an L1-free stack from separate builder and client snapshot datadirs.
    pub async fn start(config: SnapshotL2StackConfig) -> Result<Self> {
        let mut stack = Self::start_sequencer(config).await?;
        stack.start_validator().await?;
        stack.wait_for_validator().await?;
        Ok(stack)
    }

    /// Starts only the sequencing phase of an L1-free snapshot stack.
    pub async fn start_sequencer(config: SnapshotL2StackConfig) -> Result<Self> {
        let container = config.container_config.as_ref();
        let prefund = config
            .snapshot
            .prefund
            .map(|value| StandalonePrefund { address: value.address, amount: value.amount });
        let block_interval = config.snapshot.block_interval;
        let canonical_rollup_config = Arc::new(ChainConfig::mainnet().rollup_config());
        let first_block_timestamp = Self::schedule_anchor(SystemTime::now())?;
        let chain_spec = Arc::new(Self::chain_spec(first_block_timestamp, block_interval));
        let jwt_secret = JwtSecret::random();

        let builder = InProcessBuilder::start(InProcessBuilderConfig {
            chain_spec: Arc::clone(&chain_spec),
            datadir: Some(config.snapshot.builder_datadir),
            jwt_secret,
            http_port: container.and_then(|value| value.builder_http_port),
            ws_port: container.and_then(|value| value.builder_ws_port),
            auth_port: container.and_then(|value| value.builder_auth_port),
            p2p_port: container.and_then(|value| value.builder_p2p_port),
            flashblocks_port: container.and_then(|value| value.builder_flashblocks_port),
            metrics_port: None,
            block_time: block_interval.duration(),
            enable_experimental_validity_transactions: false,
            payload_builder_cutover: block_interval == DevnetBlockInterval::TwoHundredMilliseconds,
            extra_extensions: Vec::new(),
            persistence_threshold: Some(0),
            txpool_max_transactions: Some(150_000),
            txpool_max_size_mb: Some(1_024),
            txpool_max_account_slots: Some(1_024),
        })
        .await
        .wrap_err("failed to start snapshot builder")?;
        let boundary = SnapshotBoundary::read(
            builder.rpc_url()?,
            canonical_rollup_config,
            config.snapshot.expected_chain_id,
            config.snapshot.expected_head,
        )
        .await
        .wrap_err("snapshot preflight failed")?;
        ensure!(
            first_block_timestamp > boundary.head.timestamp,
            "snapshot boundary timestamp must be earlier than the local schedule anchor"
        );
        Self::ensure_schedule_anchor_is_future(first_block_timestamp, SystemTime::now())?;
        let rollup_config = Arc::new(Self::anchored_rollup_config(
            boundary.head.number,
            first_block_timestamp,
            block_interval,
        )?);

        let client = InProcessClient::start(InProcessClientConfig {
            chain_spec: ChainSpecSource::Parsed(chain_spec),
            datadir: Some(config.snapshot.client_datadir),
            jwt_secret,
            builder_rpc_url: builder.rpc_url()?.to_string(),
            // Snapshot validation replays canonical payloads only after sequencing finishes.
            builder_flashblocks_url: None,
            builder_p2p_enode: builder.p2p_enode(),
            http_port: container.and_then(|value| value.client_http_port),
            ws_port: container.and_then(|value| value.client_ws_port),
            auth_port: container.and_then(|value| value.client_auth_port),
            p2p_port: container.and_then(|value| value.client_p2p_port),
            metrics_port: None,
            persistence_threshold: Some(0),
            tx_forwarding_config: None,
            upgrade_signal: None,
            enable_experimental_validity_transactions: false,
            extra_extensions: Vec::new(),
        })
        .await
        .wrap_err("failed to start snapshot client")?;

        let unused_l1_url = Url::parse("http://127.0.0.1:1").expect("valid unused L1 URL");
        let follow_config = InProcessFollowConsensusConfig {
            rollup_config: rollup_config.as_ref().clone(),
            jwt_secret,
            l1_rpc_url: unused_l1_url,
            local_l2_rpc_url: client.rpc_url()?,
            source_l2_rpc_url: builder.rpc_url()?,
            l2_engine_url: client.engine_url()?,
            rpc_port: container.and_then(|value| value.client_consensus_rpc_port),
            // Keep catch-up observable by the per-block Prometheus scraper. This does not alter
            // measured execution latency; it only prevents multiple canonical inserts between
            // scrapes while the validator processes the already-produced range.
            insert_delay: Duration::from_millis(100),
            upgrade_signal: None,
        };
        let mut standalone_consensus =
            InProcessStandaloneSequencer::start(InProcessStandaloneSequencerConfig {
                rollup_config: rollup_config.as_ref().clone(),
                jwt_secret,
                l2_engine_url: builder.engine_url()?,
                l1_info: boundary.l1_info,
                system_config: boundary.system_config,
                prefund,
            })
            .await
            .wrap_err("failed to start snapshot standalone consensus")?;

        let target = boundary
            .head
            .number
            .checked_add(2)
            .ok_or_else(|| eyre::eyre!("snapshot boundary block number overflow"))?;
        tokio::select! {
            result = Self::wait_for_block(builder.rpc_url()?, target, Duration::from_secs(30)) => {
                result.wrap_err("snapshot builder did not produce two descendants")?;
            }
            error = standalone_consensus.next_error() => {
                eyre::bail!("standalone consensus failed before advancing snapshot: {error}");
            }
        }

        Ok(Self {
            builder,
            boundary,
            standalone_consensus: Some(standalone_consensus),
            client,
            follow_consensus: None,
            follow_config: Some(follow_config),
            block_interval,
        })
    }

    /// Stops block production while leaving the builder RPC available as validator input.
    pub async fn stop_sequencer(&mut self) -> Result<()> {
        let consensus = self
            .standalone_consensus
            .take()
            .ok_or_else(|| eyre::eyre!("snapshot sequencer is not running"))?;
        consensus.shutdown().await;
        Ok(())
    }

    /// Starts the full validator so it can process the sequencer's canonical range.
    pub async fn start_validator(&mut self) -> Result<()> {
        ensure!(self.follow_consensus.is_none(), "snapshot validator is already running");
        let config = self
            .follow_config
            .take()
            .ok_or_else(|| eyre::eyre!("snapshot validator configuration is unavailable"))?;
        let follow_consensus = InProcessFollowConsensus::start(config)
            .await
            .wrap_err("failed to start snapshot follow consensus")?;
        self.follow_consensus = Some(follow_consensus);
        Ok(())
    }

    /// Waits until the validator reaches the sequencer's current canonical head.
    pub async fn wait_for_validator(&self) -> Result<()> {
        let target =
            RootProvider::<Base>::new_http(self.builder.rpc_url()?).get_block_number().await?;
        Self::wait_for_block(self.client.rpc_url()?, target, Duration::from_secs(30))
            .await
            .wrap_err("snapshot validator did not catch up to the sequencer")
    }

    /// Anchors the deterministic local block schedule at the first post-snapshot block.
    pub fn anchored_rollup_config(
        boundary_number: u64,
        first_block_timestamp: u64,
        block_interval: DevnetBlockInterval,
    ) -> Result<RollupConfig> {
        let mut config = ChainConfig::mainnet().rollup_config();
        let first_block_number = boundary_number
            .checked_add(1)
            .ok_or_else(|| eyre::eyre!("snapshot boundary block number overflow"))?;
        let blocks_since_genesis = first_block_number
            .checked_sub(config.genesis.l2.number)
            .ok_or_else(|| eyre::eyre!("snapshot boundary predates configured L2 genesis"))?;
        let legacy_elapsed = blocks_since_genesis
            .checked_mul(config.block_time)
            .ok_or_else(|| eyre::eyre!("snapshot schedule offset overflow"))?;
        config.genesis.l2_time = first_block_timestamp
            .checked_sub(legacy_elapsed)
            .ok_or_else(|| eyre::eyre!("snapshot is too far ahead to anchor locally"))?;
        if block_interval == DevnetBlockInterval::TwoHundredMilliseconds {
            config.set_upgrade_activation_timestamp(BaseUpgrade::Denim, first_block_timestamp);
        }
        let (planned_timestamp, planned_millis) =
            config.l2_block_timestamp_parts(first_block_number);
        ensure!(
            (planned_timestamp, planned_millis) == (first_block_timestamp, 0),
            "failed to anchor first local snapshot descendant"
        );
        Ok(config)
    }

    /// Builds the execution chain specification for a local snapshot schedule.
    pub fn chain_spec(
        first_block_timestamp: u64,
        block_interval: DevnetBlockInterval,
    ) -> BaseChainSpec {
        let mut chain_spec = BaseChainSpec::mainnet();
        if block_interval == DevnetBlockInterval::TwoHundredMilliseconds {
            chain_spec
                .set_fork(BaseUpgrade::Denim, ForkCondition::Timestamp(first_block_timestamp));
        }
        chain_spec
    }

    fn schedule_anchor(now: SystemTime) -> Result<u64> {
        now.duration_since(UNIX_EPOCH)
            .wrap_err("system clock is before Unix epoch")?
            .as_secs()
            .checked_add(SNAPSHOT_STARTUP_LEAD.as_secs())
            .ok_or_else(|| eyre::eyre!("snapshot schedule anchor overflow"))
    }

    fn ensure_schedule_anchor_is_future(first_block_timestamp: u64, now: SystemTime) -> Result<()> {
        let now =
            now.duration_since(UNIX_EPOCH).wrap_err("system clock is before Unix epoch")?.as_secs();
        ensure!(
            first_block_timestamp > now,
            "snapshot builder startup exhausted the {}s schedule lead; retry or increase the lead",
            SNAPSHOT_STARTUP_LEAD.as_secs()
        );
        Ok(())
    }

    async fn wait_for_block(rpc_url: Url, target: u64, timeout: Duration) -> Result<()> {
        let provider = RootProvider::<Base>::new_http(rpc_url);
        tokio::time::timeout(timeout, async {
            loop {
                let block_number = provider.get_block_number().await?;
                if block_number >= target {
                    return Ok::<_, eyre::Report>(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .wrap_err("timed out waiting for snapshot chain advancement")??;
        Ok(())
    }

    /// Returns the validated immutable snapshot boundary.
    pub const fn boundary(&self) -> &SnapshotBoundary {
        &self.boundary
    }

    /// Returns the configured interval between locally produced blocks.
    pub const fn block_interval(&self) -> DevnetBlockInterval {
        self.block_interval
    }

    /// Returns the builder RPC URL.
    pub fn builder_rpc_url(&self) -> Result<Url> {
        self.builder.rpc_url()
    }

    /// Returns the client RPC URL.
    pub fn client_rpc_url(&self) -> Result<Url> {
        self.client.rpc_url()
    }

    /// Returns the builder Prometheus metrics URL.
    pub fn builder_metrics_url(&self) -> Result<Url> {
        self.builder.metrics_url()
    }

    /// Returns the client Prometheus metrics URL.
    pub fn client_metrics_url(&self) -> Result<Url> {
        self.client.metrics_url()
    }

    /// Returns the builder Flashblocks WebSocket URL.
    pub fn builder_flashblocks_url(&self) -> Result<Url> {
        Url::parse(&self.builder.flashblocks_url()).wrap_err("invalid builder Flashblocks URL")
    }

    /// Reads the builder's current head using the same boundary decoder as startup preflight.
    pub async fn current_builder_boundary(&self) -> Result<SnapshotBoundary> {
        SnapshotBoundary::read(
            self.builder.rpc_url()?,
            Arc::new(ChainConfig::mainnet().rollup_config()),
            ChainConfig::mainnet().chain_id,
            None,
        )
        .await
    }

    /// Verifies both ELs currently report the same block number.
    pub async fn ensure_heads_match(&self) -> Result<()> {
        let builder = RootProvider::<Base>::new_http(self.builder.rpc_url()?);
        let client = RootProvider::<Base>::new_http(self.client.rpc_url()?);
        let (builder_head, client_head) =
            tokio::try_join!(builder.get_block_number(), client.get_block_number())?;
        ensure!(builder_head == client_head, "builder head {builder_head} != client {client_head}");
        Ok(())
    }

    /// Gracefully stops consensus tasks before dropping their execution nodes.
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(consensus) = self.standalone_consensus.take() {
            consensus.shutdown().await;
        }
        if let Some(consensus) = self.follow_consensus.take() {
            consensus.shutdown().await;
        }
        tokio::try_join!(self.builder.shutdown(), self.client.shutdown())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use base_common_chains::Upgrades;
    use base_common_genesis::BaseUpgrade;
    use reth_ethereum_forks::ForkCondition;

    use super::SnapshotL2Stack;
    use crate::DevnetBlockInterval;

    #[test]
    fn anchors_two_second_schedule_at_first_descendant() {
        let config = SnapshotL2Stack::anchored_rollup_config(
            30_000_000,
            2_000_000_000,
            DevnetBlockInterval::TwoSeconds,
        )
        .unwrap();

        assert_eq!(config.l2_block_timestamp_parts(30_000_001), (2_000_000_000, 0));
        assert_eq!(config.l2_block_timestamp_parts(30_000_002), (2_000_000_002, 0));
    }

    #[test]
    fn anchors_subsecond_schedule_with_rollover() {
        let config = SnapshotL2Stack::anchored_rollup_config(
            30_000_000,
            2_000_000_000,
            DevnetBlockInterval::TwoHundredMilliseconds,
        )
        .unwrap();

        assert_eq!(config.l2_block_timestamp_parts(30_000_001), (2_000_000_000, 0));
        assert_eq!(config.l2_block_timestamp_parts(30_000_002), (2_000_000_000, 200));
        assert_eq!(config.l2_block_timestamp_parts(30_000_005), (2_000_000_000, 800));
        assert_eq!(config.l2_block_timestamp_parts(30_000_006), (2_000_000_001, 0));
    }

    #[test]
    fn subsecond_cl_and_el_activate_denim_at_same_timestamp() {
        let activation = 2_000_000_000;
        let rollup = SnapshotL2Stack::anchored_rollup_config(
            30_000_000,
            activation,
            DevnetBlockInterval::TwoHundredMilliseconds,
        )
        .unwrap();
        let chain_spec =
            SnapshotL2Stack::chain_spec(activation, DevnetBlockInterval::TwoHundredMilliseconds);

        assert!(!rollup.is_denim_active(activation - 1));
        assert!(rollup.is_denim_active(activation));
        assert!(!chain_spec.is_denim_active_at_timestamp(activation - 1));
        assert!(chain_spec.is_denim_active_at_timestamp(activation));
        assert_eq!(chain_spec.fork(BaseUpgrade::Denim), ForkCondition::Timestamp(activation));
    }

    #[test]
    fn schedule_anchor_uses_startup_lead() {
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000);

        let anchor = SnapshotL2Stack::schedule_anchor(now).unwrap();

        assert_eq!(anchor, 2_000_000_000 + 10);
        SnapshotL2Stack::ensure_schedule_anchor_is_future(anchor, now).unwrap();
    }

    #[test]
    fn schedule_anchor_rejects_exhausted_lead() {
        let anchor = 2_000_000_010;
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(anchor);

        let error = SnapshotL2Stack::ensure_schedule_anchor_is_future(anchor, now)
            .expect_err("anchor at current time should be stale");

        assert!(error.to_string().contains("exhausted"));
    }
}
