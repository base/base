//! Persistent L1-free development chain over an execution node's snapshot datadir.

use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::Header;
use alloy_eips::{BlockNumHash, BlockNumberOrTag};
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, JwtSecret};
use base_common_chains::BaseUpgradeExt;
use base_common_consensus::REGOLITH_SYSTEM_TX_GAS;
use base_common_genesis::{BaseUpgrade, RollupConfig, RuntimeUpgradeRegistry};
use base_common_network::BaseEngineApi;
use base_consensus_engine::{EngineClient, EngineClientError};
use base_protocol::{BaseBlockConversionError, BaseTimeUpdateTx, L2BlockMetadata};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

use crate::{
    EngineConfig, NodeOperatingMode, ShutdownSignal, StandaloneDenimSchedule,
    StandaloneImpersonationQueue, StandaloneScheduleError, StandaloneSequencerNode,
};

/// Development schedule and snapshot identity persisted with a datadir.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandaloneDevState {
    /// L2 chain ID of the datadir.
    pub l2_chain_id: u64,
    /// Genesis block hash of the datadir.
    pub genesis_hash: B256,
    /// Snapshot head extended by the first development block.
    pub boundary: BlockNumHash,
    /// Denim activation timestamp of the development schedule.
    pub denim_timestamp: u64,
}

impl StandaloneDevState {
    /// Durably replaces the state file at `path`.
    pub fn persist(&self, path: &Path) -> io::Result<()> {
        let temporary = path.with_extension("json.tmp");
        let mut file = File::create(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()
    }
}

/// L1-free development chain extending the head of an execution node's snapshot datadir.
///
/// The first run chooses the Denim schedule from the head and persists it with the head's identity
/// before producing a block. Later runs restore that schedule rather than choosing a new one, and
/// reject a datadir whose canonical chain no longer matches it.
#[derive(Debug)]
pub struct StandaloneDevChain {
    /// Canonical rollup configuration of the datadir's chain.
    pub rollup_config: RollupConfig,
    /// File holding the persisted [`StandaloneDevState`].
    pub state_path: PathBuf,
    /// State restored from `state_path`; absent before the first run.
    pub state: Option<StandaloneDevState>,
}

impl StandaloneDevChain {
    /// Name of the state file inside the datadir.
    pub const STATE_FILE: &str = "base-dev.json";

    /// Interval between checks for the execution node to finish its startup sync.
    pub const EL_SYNC_POLL_INTERVAL: Duration = Duration::from_secs(1);

    /// Restores persisted state from `datadir` and installs its Denim schedule process-wide.
    ///
    /// Call this before the execution node starts so it never handles development blocks without
    /// their schedule.
    pub fn open(datadir: &Path, rollup_config: RollupConfig) -> Result<Self, StandaloneDevError> {
        let state_path = datadir.join(Self::STATE_FILE);
        let state = match fs::read(&state_path) {
            Ok(bytes) => {
                Some(serde_json::from_slice::<StandaloneDevState>(&bytes).map_err(|source| {
                    StandaloneDevError::StateFormat { path: state_path.clone(), source }
                })?)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => return Err(StandaloneDevError::StateIo { path: state_path, source }),
        };
        if let Some(state) = state {
            let chain_id = rollup_config.l2_chain_id.id();
            if state.l2_chain_id != chain_id {
                return Err(StandaloneDevError::StateMismatch {
                    path: state_path,
                    field: "chain ID",
                });
            }
            RuntimeUpgradeRegistry::set_activation_timestamp(
                chain_id,
                BaseUpgrade::Denim,
                state.denim_timestamp,
            );
        }
        Ok(Self { rollup_config, state_path, state })
    }

    /// Validates the schedule against the canonical chain, persists it before the first
    /// development block, and sequences until `cancellation` is triggered.
    ///
    /// `header_by_number` must read canonical headers from the already-open execution provider;
    /// historical block bodies may have been pruned.
    ///
    /// `impersonation` starts accepting requests once the schedule is known, and its deposits are
    /// sequenced after the system deposits of each block.
    ///
    /// Returns an error if the sequencer stops for any other reason.
    pub async fn run<E: std::fmt::Display>(
        self,
        engine_url: Url,
        jwt_secret: JwtSecret,
        header_by_number: impl Fn(u64) -> Result<Option<Header>, E>,
        impersonation: StandaloneImpersonationQueue,
        cancellation: CancellationToken,
    ) -> Result<(), StandaloneDevError> {
        let chain_id = self.rollup_config.l2_chain_id.id();
        let mismatch =
            |field| StandaloneDevError::StateMismatch { path: self.state_path.clone(), field };
        let engine = EngineConfig {
            config: Arc::new(self.rollup_config.clone()),
            l2_url: engine_url,
            l2_jwt_secret: jwt_secret,
            // Standalone sequencing never issues an L1 request.
            l1_url: Url::parse("http://127.0.0.1:1").expect("valid unused L1 URL"),
            l1_rpc_timeout: base_consensus_providers::L1_RPC_TIMEOUT,
            mode: NodeOperatingMode::Sequencer,
        }
        .build_engine_client()
        .await
        .map_err(EngineClientError::from)?;

        let genesis_hash = header_by_number(0)
            .map_err(|error| StandaloneDevError::HeaderRead(error.to_string()))?
            .ok_or(StandaloneDevError::MissingBlock(BlockNumberOrTag::Number(0)))?
            .hash_slow();
        if let Some(state) = self.state {
            if state.genesis_hash != genesis_hash {
                return Err(mismatch("genesis hash"));
            }
            let boundary = header_by_number(state.boundary.number)
                .map_err(|error| StandaloneDevError::HeaderRead(error.to_string()))?
                .filter(|header| header.hash_slow() == state.boundary.hash)
                .ok_or_else(|| mismatch("boundary block"))?;
            // Use the original chain configuration, not the runtime override restored from the file.
            let expected_activation = self
                .rollup_config
                .upgrades
                .activation(BaseUpgrade::Denim)
                .timestamp()
                .filter(|activation| *activation <= boundary.timestamp)
                .unwrap_or(boundary.timestamp + self.rollup_config.block_time);
            if state.denim_timestamp != expected_activation {
                return Err(mismatch("Denim schedule"));
            }
        }

        let head = engine
            .l2_block_by_label(BlockNumberOrTag::Latest)
            .await?
            .ok_or(StandaloneDevError::MissingBlock(BlockNumberOrTag::Latest))?
            .map_header(|header| header.into_inner())
            .into_consensus()
            .map_transactions(|transaction| transaction.inner.inner.into_inner());
        let head_id = BlockNumHash { number: head.header.number, hash: head.header.hash_slow() };
        if head_id.number == 0 {
            return Err(StandaloneDevError::GenesisHead);
        }
        // Only Denim blocks carry BaseTime, so it identifies development blocks even when their
        // whole-second timestamps fit the legacy schedule.
        let head_millis = BaseTimeUpdateTx::extract_timestamp_ms(
            &head.body.transactions,
            head_id.number,
            head.header.timestamp,
        )
        .ok();
        if self.state.is_none()
            && head_millis.is_some()
            && !self.rollup_config.is_denim_active(head.header.timestamp)
        {
            return Err(StandaloneDevError::MissingState {
                path: self.state_path,
                head: head_id.number,
            });
        }
        let metadata = L2BlockMetadata::from_block(&head, &self.rollup_config)?;
        let schedule = StandaloneDenimSchedule::new(self.rollup_config, &metadata).map_err(
            |error| match (self.state, error) {
                (Some(_), StandaloneScheduleError::HeadTimestamp { .. }) => {
                    mismatch("Denim schedule")
                }
                _ => error.into(),
            },
        )?;
        let rollup_config = schedule.rollup_config;
        let denim_timestamp = rollup_config
            .upgrade_activation_timestamp(BaseUpgrade::Denim)
            .expect("standalone schedules always activate Denim");
        let scheduled_millis = rollup_config
            .is_denim_active(head.header.timestamp)
            .then(|| rollup_config.l2_block_timestamp_millis(head_id.number));
        if head_millis != scheduled_millis {
            return Err(mismatch("Denim schedule"));
        }
        let boundary = match self.state {
            Some(state) => state.boundary,
            None => {
                StandaloneDevState {
                    l2_chain_id: chain_id,
                    genesis_hash,
                    boundary: head_id,
                    denim_timestamp,
                }
                .persist(&self.state_path)
                .map_err(|source| StandaloneDevError::StateIo {
                    path: self.state_path.clone(),
                    source,
                })?;
                head_id
            }
        };
        RuntimeUpgradeRegistry::set_activation_timestamp(
            chain_id,
            BaseUpgrade::Denim,
            denim_timestamp,
        );

        // The execution node answers `Syncing` while it runs pipeline stages on startup, such as
        // rebuilding the indices a snapshot download resets. Anchoring the schedule before then
        // would leave every block scheduled during the rebuild overdue.
        let forkchoice = ForkchoiceState { head_block_hash: head_id.hash, ..Default::default() };
        while engine
            .fork_choice_updated_v3(forkchoice, None)
            .await
            .map_err(EngineClientError::from)?
            .payload_status
            .status
            .is_syncing()
        {
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                () = tokio::time::sleep(Self::EL_SYNC_POLL_INTERVAL) => {}
            }
        }

        let head_time = UNIX_EPOCH
            + Duration::from_millis(rollup_config.l2_block_timestamp_millis(head_id.number));
        let schedule_delay = SystemTime::now().duration_since(head_time).unwrap_or_default();
        info!(
            target: "standalone_dev",
            head = head_id.number,
            head_hash = %head_id.hash,
            boundary = boundary.number,
            denim_timestamp,
            state = %self.state_path.display(),
            "starting development chain"
        );
        warn!(
            target: "standalone_dev",
            behind_wall_clock_secs = schedule_delay.as_secs(),
            "block timestamps continue from the snapshot, not wall-clock time"
        );

        // Every development block also carries the L1-info and BaseTime system deposits.
        impersonation.activate(
            schedule.system_config.gas_limit.saturating_sub(2 * REGOLITH_SYSTEM_TX_GAS),
            BaseUpgrade::from_timestamp(
                &rollup_config,
                rollup_config.l2_block_timestamp(head_id.number + 1),
            ),
        );
        let mut node = StandaloneSequencerNode::new(
            Arc::new(rollup_config),
            Arc::new(engine),
            metadata.l1_info,
            schedule.system_config,
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
    /// The state file could not be read or written.
    #[error("development state {}: {source}", path.display())]
    StateIo {
        /// State file path.
        path: PathBuf,
        /// I/O error.
        source: io::Error,
    },
    /// The state file is malformed.
    #[error("development state {} is malformed: {source}", path.display())]
    StateFormat {
        /// State file path.
        path: PathBuf,
        /// Decoding error.
        source: serde_json::Error,
    },
    /// The state file does not describe this datadir's canonical chain.
    #[error("development state {} does not match this datadir: {field} differs", path.display())]
    StateMismatch {
        /// State file path.
        path: PathBuf,
        /// Mismatched property.
        field: &'static str,
    },
    /// The head is a development block but the state file is missing.
    #[error(
        "head {head} is a development block but {} is missing; restore it from a backup of this datadir",
        path.display()
    )]
    MissingState {
        /// Expected state file path.
        path: PathBuf,
        /// Head block number.
        head: u64,
    },
    /// The execution engine request failed.
    #[error("execution engine request failed: {0}")]
    Engine(#[from] EngineClientError),
    /// A canonical header could not be read from the execution provider.
    #[error("cannot read development chain identity: {0}")]
    HeaderRead(String),
    /// The execution node does not have a required block.
    #[error("execution node has no {0} block")]
    MissingBlock(BlockNumberOrTag),
    /// The datadir contains only the genesis block.
    #[error("datadir contains only genesis; use a snapshot datadir")]
    GenesisHead,
    /// The head lacks the metadata needed to extend it.
    #[error("cannot extend snapshot head: {0}")]
    Head(#[from] BaseBlockConversionError),
    /// No valid Denim schedule extends the head.
    #[error(transparent)]
    Schedule(#[from] StandaloneScheduleError),
    /// The sequencer failed.
    #[error("standalone sequencer failed: {0}")]
    Sequencer(String),
    /// The sequencer stopped without a shutdown request.
    #[error("standalone sequencer stopped unexpectedly")]
    Stopped,
}
