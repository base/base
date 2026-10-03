//! L1-free fresh-genesis L2 stack: one builder execution node and a standalone sequencer.

use std::{
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_eips::{BlockNumHash, eip1559::BaseFeeParams};
use alloy_primitives::{B64, B256, keccak256};
use alloy_provider::RootProvider;
use alloy_rpc_types_engine::JwtSecret;
use base_common_chains::ChainConfig;
use base_common_consensus::JovianExtraData;
use base_common_genesis::{RollupConfig, SystemConfig};
use base_common_network::Base;
use base_consensus_node::StandalonePrefund;
use base_execution_chainspec::BaseChainSpec;
use base_protocol::{L1BlockInfoJovian, L1BlockInfoTx};
use eyre::{Result, WrapErr, ensure};
use url::Url;

use super::{
    InProcessBuilder, InProcessBuilderConfig, InProcessNodeRuntime, InProcessStandaloneSequencer,
    InProcessStandaloneSequencerConfig,
};
use crate::{DevnetBlockInterval, DevnetPrefund, SystemTestPorts, SystemTestProviderExt};

/// Delay between process start and the L2 genesis timestamp.
///
/// The sequencer produces blocks on the wall clock, so the genesis block must not be in the past
/// or the sequencer would mint the backlog in a burst. The lead covers builder startup.
const LIGHT_STARTUP_LEAD: Duration = Duration::from_secs(5);

/// Maximum age of a caller-supplied genesis before it is rejected as stale.
const LIGHT_MAX_GENESIS_AGE: Duration = Duration::from_secs(60 * 60);

/// Time allowed for the builder RPC to answer after launch.
const LIGHT_EL_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Time allowed for the first local blocks to appear after the sequencer starts.
const LIGHT_ADVANCE_TIMEOUT: Duration = Duration::from_secs(60);

/// Number of blocks the stack waits for before reporting itself ready.
const LIGHT_READY_BLOCK: u64 = 2;

/// Legacy block interval, in seconds, that the generated rollup configuration schedules.
const LIGHT_LEGACY_BLOCK_TIME_SECS: u64 = 2;

/// Minimum base fee of the generated chain, matching the Docker devnet intent.
const LIGHT_MIN_BASE_FEE: u64 = 1_000_000_000;

/// Default block gas limit of the generated chain with two-second blocks.
const LIGHT_GAS_LIMIT_2S: u64 = 60_000_000;

/// Seed hashed into the synthetic L1 origin that every L2 block of the light devnet references.
const LIGHT_L1_ORIGIN_SEED: &[u8] = b"base-devnet-light-l1-origin";

/// Base fee of the synthetic L1 origin, in wei.
const LIGHT_L1_BASE_FEE: u64 = 1_000_000_000;

/// Blob base fee of the synthetic L1 origin, in wei.
const LIGHT_L1_BLOB_BASE_FEE: u128 = 1;

/// Caller-supplied genesis inputs that replace the generated chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LightGenesisFiles {
    /// Path to an L2 genesis JSON file.
    pub l2_genesis: PathBuf,
    /// Path to the matching rollup configuration JSON file.
    pub rollup_config: PathBuf,
}

/// Configuration for an L1-free fresh-genesis L2 stack.
#[derive(Debug, Clone)]
pub struct LightL2StackConfig {
    /// Runtime sizing policy for the builder execution node.
    pub runtime: InProcessNodeRuntime,
    /// Interval between locally produced blocks.
    pub block_interval: DevnetBlockInterval,
    /// Block gas limit of the generated chain. Defaults to [`LightChain::default_gas_limit`].
    pub block_gas_limit: Option<u64>,
    /// Caller-owned builder datadir. A temporary datadir removed on drop is used when omitted.
    pub datadir: Option<PathBuf>,
    /// Stable port assignments. Free ports are allocated when omitted.
    pub ports: Option<SystemTestPorts>,
    /// Optional one-time account funding applied in the first block.
    pub prefund: Option<DevnetPrefund>,
    /// Caller-supplied genesis files replacing the generated chain.
    pub genesis_files: Option<LightGenesisFiles>,
    /// Pending/basefee/queued transaction count limit.
    pub txpool_max_transactions: usize,
    /// Pending/basefee/queued transaction size limit in megabytes.
    pub txpool_max_size_mb: usize,
    /// Maximum number of transaction slots retained per sender.
    pub txpool_max_account_slots: usize,
}

impl Default for LightL2StackConfig {
    fn default() -> Self {
        Self {
            runtime: InProcessNodeRuntime::Host,
            block_interval: DevnetBlockInterval::default(),
            block_gas_limit: None,
            datadir: None,
            ports: None,
            prefund: None,
            genesis_files: None,
            txpool_max_transactions: 50_000,
            txpool_max_size_mb: 256,
            txpool_max_account_slots: 1_024,
        }
    }
}

/// The execution chain, rollup configuration, and sequencing seed of a light devnet.
#[derive(Debug, Clone)]
pub struct LightChain {
    /// Execution chain specification.
    pub chain_spec: Arc<BaseChainSpec>,
    /// Rollup configuration whose genesis matches [`Self::chain_spec`].
    pub rollup_config: Arc<RollupConfig>,
    /// System configuration applied to every locally produced block.
    pub system_config: SystemConfig,
    /// L1-info transaction describing the synthetic L1 origin of the genesis block.
    pub l1_info: L1BlockInfoTx,
}

impl LightChain {
    /// Returns the default block gas limit for the given block interval.
    ///
    /// Both cadences expose thirty million gas per second of theoretical block capacity.
    pub const fn default_gas_limit(interval: DevnetBlockInterval) -> u64 {
        match interval {
            DevnetBlockInterval::TwoSeconds => LIGHT_GAS_LIMIT_2S,
            DevnetBlockInterval::TwoHundredMilliseconds => LIGHT_GAS_LIMIT_2S / 10,
        }
    }

    /// Generates a fresh chain from the built-in dev chain configuration.
    ///
    /// The dev genesis prefunds the Anvil test accounts and deploys the `BaseTime` predeploy.
    /// Every fork through Cobalt is active at genesis, as in the Docker devnet. Denim is
    /// scheduled at the first block when `interval` is 200ms. The genesis block is stamped with
    /// `genesis_timestamp`, and a synthetic L1 origin stands in for the absent L1.
    pub fn generated(
        genesis_timestamp: u64,
        interval: DevnetBlockInterval,
        gas_limit: u64,
    ) -> Result<Self> {
        ensure!(gas_limit > 0, "light devnet block gas limit must be greater than zero");
        let first_block_timestamp = genesis_timestamp
            .checked_add(LIGHT_LEGACY_BLOCK_TIME_SECS)
            .ok_or_else(|| eyre::eyre!("light devnet genesis timestamp overflow"))?;
        let fees = Self::fee_params(interval);

        let mut config = ChainConfig::devnet().clone();
        config.beryl_timestamp = Some(0);
        config.cobalt_timestamp = Some(0);
        config.denim_timestamp = (interval == DevnetBlockInterval::TwoHundredMilliseconds)
            .then_some(first_block_timestamp);
        config.block_time = LIGHT_LEGACY_BLOCK_TIME_SECS;
        config.genesis_l2_time = genesis_timestamp;
        config.genesis_gas_limit = gas_limit;
        config.genesis_batcher_address = crate::BATCHER.address;
        config.genesis_l1_hash = keccak256(LIGHT_L1_ORIGIN_SEED);

        let mut chain_spec = BaseChainSpec::try_from(&config)
            .wrap_err("failed to build the light devnet chain specification")?;
        chain_spec.inner.genesis.timestamp = genesis_timestamp;
        chain_spec.inner.genesis.gas_limit = gas_limit;
        chain_spec.inner.genesis.extra_data = JovianExtraData::encode(
            B64::ZERO,
            BaseFeeParams::new(fees.denominator.into(), fees.elasticity.into()),
            LIGHT_MIN_BASE_FEE,
        )
        .wrap_err("failed to encode the genesis fee parameters")?;
        chain_spec.refresh_genesis_header();
        config.genesis_l2_hash = chain_spec.genesis_hash();

        let rollup_config = config.rollup_config();
        let system_config = SystemConfig {
            gas_limit,
            eip1559_denominator: Some(fees.denominator),
            eip1559_elasticity: Some(fees.elasticity),
            min_base_fee: Some(LIGHT_MIN_BASE_FEE),
            ..rollup_config.genesis.system_config.unwrap_or_default()
        };
        Self::assemble(chain_spec, rollup_config, system_config, interval)
    }

    /// Loads a chain from caller-supplied genesis and rollup configuration JSON.
    ///
    /// The genesis must be recent: the sequencer follows the wall clock, so a genesis older than
    /// an hour is rejected rather than replayed in a burst. The rollup configuration must
    /// describe the supplied genesis block, and its Denim schedule must agree with `interval`.
    pub fn from_json(
        l2_genesis: &[u8],
        rollup_config: &[u8],
        interval: DevnetBlockInterval,
        now: SystemTime,
    ) -> Result<Self> {
        let chain_spec = InProcessBuilderConfig::chain_spec_from_genesis_json(l2_genesis)?;
        let rollup_config: RollupConfig =
            serde_json::from_slice(rollup_config).wrap_err("failed to parse rollup config")?;
        ensure!(
            rollup_config.l2_chain_id.id() == chain_spec.chain.id(),
            "rollup config L2 chain ID {} does not match genesis chain ID {}",
            rollup_config.l2_chain_id.id(),
            chain_spec.chain.id()
        );
        ensure!(rollup_config.genesis.l2.number == 0, "light devnet requires an L2 genesis at 0");
        ensure!(
            rollup_config.genesis.l2.hash == chain_spec.genesis_hash(),
            "rollup config L2 genesis hash {} does not match genesis block hash {}",
            rollup_config.genesis.l2.hash,
            chain_spec.genesis_hash()
        );
        ensure!(
            rollup_config.genesis.l2_time == chain_spec.genesis_header().timestamp,
            "rollup config genesis time does not match the genesis block timestamp"
        );
        let now_secs =
            now.duration_since(UNIX_EPOCH).wrap_err("system clock is before Unix epoch")?.as_secs();
        ensure!(
            rollup_config.genesis.l2_time.saturating_add(LIGHT_MAX_GENESIS_AGE.as_secs())
                >= now_secs,
            "genesis timestamp {} is more than {}s old; regenerate it so the sequencer does not \
             replay the backlog",
            rollup_config.genesis.l2_time,
            LIGHT_MAX_GENESIS_AGE.as_secs()
        );

        let fees = Self::fee_params(interval);
        let chain_spec = chain_spec.as_ref().clone();
        let mut system_config = rollup_config
            .genesis
            .system_config
            .ok_or_else(|| eyre::eyre!("rollup config has no genesis system config"))?;
        system_config.gas_limit = chain_spec.genesis_header().gas_limit;
        system_config.eip1559_denominator.get_or_insert(fees.denominator);
        system_config.eip1559_elasticity.get_or_insert(fees.elasticity);
        Self::assemble(chain_spec, rollup_config, system_config, interval)
    }

    /// Returns the block gas limit of locally produced blocks.
    pub const fn block_gas_limit(&self) -> u64 {
        self.system_config.gas_limit
    }

    /// Returns the L2 chain ID.
    pub fn chain_id(&self) -> u64 {
        self.chain_spec.chain.id()
    }

    /// Returns the timestamp, in seconds, of the first locally produced block.
    pub fn first_block_timestamp(&self) -> u64 {
        self.rollup_config.l2_block_timestamp(1)
    }

    /// Validates the pair and derives the synthetic L1 origin from the rollup genesis.
    fn assemble(
        chain_spec: BaseChainSpec,
        rollup_config: RollupConfig,
        system_config: SystemConfig,
        interval: DevnetBlockInterval,
    ) -> Result<Self> {
        let first_block_timestamp = rollup_config.l2_block_timestamp(1);
        ensure!(
            rollup_config.is_jovian_active(first_block_timestamp),
            "light devnet requires Jovian to be active at the first block"
        );
        ensure!(
            rollup_config.is_denim_active(first_block_timestamp)
                == (interval == DevnetBlockInterval::TwoHundredMilliseconds),
            "the rollup config's Denim schedule does not match the {}ms block interval",
            interval.duration().as_millis()
        );
        let l1_info = Self::genesis_l1_info(&rollup_config, &system_config);
        Ok(Self {
            chain_spec: Arc::new(chain_spec),
            rollup_config: Arc::new(rollup_config),
            system_config,
            l1_info,
        })
    }

    /// Builds the L1-info transaction describing the rollup's genesis L1 block.
    const fn genesis_l1_info(
        rollup_config: &RollupConfig,
        system_config: &SystemConfig,
    ) -> L1BlockInfoTx {
        let BlockNumHash { number, hash } = rollup_config.genesis.l1;
        L1BlockInfoTx::Jovian(L1BlockInfoJovian::new(
            number,
            rollup_config.genesis.l2_time,
            LIGHT_L1_BASE_FEE,
            hash,
            0,
            system_config.batcher_address,
            LIGHT_L1_BLOB_BASE_FEE,
            0,
            0,
            0,
            0,
            L1BlockInfoJovian::DEFAULT_DA_FOOTPRINT_GAS_SCALAR,
        ))
    }

    /// Returns the EIP-1559 parameters, scaled for the block interval as Denim scales them.
    pub const fn fee_params(interval: DevnetBlockInterval) -> LightFeeParams {
        let denominator = ChainConfig::devnet().eip1559_denominator_canyon as u32;
        let scale = match interval {
            DevnetBlockInterval::TwoSeconds => 1,
            DevnetBlockInterval::TwoHundredMilliseconds => {
                RollupConfig::DENIM_GAS_PARAMETER_SCALING_FACTOR
            }
        };
        LightFeeParams {
            denominator: denominator * scale,
            elasticity: ChainConfig::devnet().eip1559_elasticity as u32,
        }
    }
}

/// EIP-1559 parameters applied to the light devnet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LightFeeParams {
    /// Maximum base fee change denominator.
    pub denominator: u32,
    /// Elasticity multiplier.
    pub elasticity: u32,
}

/// A running L1-free builder and standalone sequencer on a fresh genesis.
#[derive(Debug)]
pub struct LightL2Stack {
    builder: InProcessBuilder,
    standalone_consensus: InProcessStandaloneSequencer,
    chain: LightChain,
    block_interval: DevnetBlockInterval,
}

impl LightL2Stack {
    /// Starts the builder and the standalone sequencer, returning once blocks are advancing.
    ///
    /// Nothing in this path leaves the local host: there is no L1, beacon node, or Docker
    /// dependency.
    pub async fn start(config: LightL2StackConfig) -> Result<Self> {
        if let Some(datadir) = &config.datadir {
            Self::ensure_fresh_datadir(datadir)?;
        }
        let block_interval = config.block_interval;
        let chain = match &config.genesis_files {
            Some(files) => {
                let l2_genesis = std::fs::read(&files.l2_genesis).wrap_err_with(|| {
                    format!("failed to read L2 genesis {}", files.l2_genesis.display())
                })?;
                let rollup_config = std::fs::read(&files.rollup_config).wrap_err_with(|| {
                    format!("failed to read rollup config {}", files.rollup_config.display())
                })?;
                LightChain::from_json(
                    &l2_genesis,
                    &rollup_config,
                    block_interval,
                    SystemTime::now(),
                )?
            }
            None => LightChain::generated(
                Self::genesis_timestamp(SystemTime::now())?,
                block_interval,
                config
                    .block_gas_limit
                    .unwrap_or_else(|| LightChain::default_gas_limit(block_interval)),
            )?,
        };
        let prefund = config
            .prefund
            .map(|value| StandalonePrefund { address: value.address, amount: value.amount });
        let ports = config.ports.as_ref();
        let jwt_secret = JwtSecret::random();

        let builder = InProcessBuilder::start(InProcessBuilderConfig {
            runtime: config.runtime,
            chain_spec: Arc::clone(&chain.chain_spec),
            datadir: config.datadir,
            require_existing_datadir: false,
            jwt_secret,
            rpc_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            p2p_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
            p2p_key: crate::BUILDER.private_key,
            http_port: ports.map(|value| value.l2_builder_http),
            ws_port: ports.map(|value| value.l2_builder_ws),
            auth_port: ports.map(|value| value.l2_builder_auth),
            p2p_port: ports.map(|value| value.l2_builder_p2p),
            flashblocks_port: ports.map(|value| value.l2_builder_flashblocks),
            metrics_port: ports.map(|value| value.l2_builder_metrics),
            block_time: block_interval.duration(),
            enable_experimental_validity_transactions: false,
            payload_builder_cutover: block_interval == DevnetBlockInterval::TwoHundredMilliseconds,
            extra_extensions: Vec::new(),
            persistence_threshold: Some(0),
            persistence_backpressure_threshold: Some(
                block_interval.persistence_backpressure_threshold(),
            ),
            txpool_max_transactions: Some(config.txpool_max_transactions),
            txpool_max_size_mb: Some(config.txpool_max_size_mb),
            txpool_max_account_slots: Some(config.txpool_max_account_slots),
            clear_otel_env: true,
        })
        .await
        .wrap_err("failed to start light devnet builder")?;
        // The first block number query doubles as a readiness probe for the builder RPC.
        let provider = RootProvider::<Base>::new_http(builder.rpc_url()?);
        provider
            .wait_for_block(0, LIGHT_EL_READY_TIMEOUT)
            .await
            .wrap_err("light devnet builder RPC did not become ready")?;

        let mut standalone_consensus =
            InProcessStandaloneSequencer::start(InProcessStandaloneSequencerConfig {
                rollup_config: chain.rollup_config.as_ref().clone(),
                jwt_secret,
                l2_engine_url: builder.engine_url()?,
                l1_info: chain.l1_info,
                system_config: chain.system_config,
                prefund,
            })
            .await
            .wrap_err("failed to start light devnet standalone consensus")?;

        tokio::select! {
            result = provider.wait_for_block(LIGHT_READY_BLOCK, LIGHT_ADVANCE_TIMEOUT) => {
                result.wrap_err("light devnet builder did not produce its first blocks")?;
            }
            error = standalone_consensus.next_error() => {
                eyre::bail!("standalone consensus failed before advancing the chain: {error}");
            }
        }

        Ok(Self { builder, standalone_consensus, chain, block_interval })
    }

    /// Returns the genesis timestamp for a chain generated at `now`.
    fn genesis_timestamp(now: SystemTime) -> Result<u64> {
        now.duration_since(UNIX_EPOCH)
            .wrap_err("system clock is before Unix epoch")?
            .as_secs()
            .checked_add(LIGHT_STARTUP_LEAD.as_secs())
            .ok_or_else(|| eyre::eyre!("light devnet genesis timestamp overflow"))
    }

    /// Rejects a datadir that already holds a database.
    ///
    /// Every run generates a genesis stamped with the current time, so an existing database would
    /// fail its genesis hash check.
    fn ensure_fresh_datadir(datadir: &Path) -> Result<()> {
        ensure!(
            !datadir.join("db/mdbx.dat").exists(),
            "datadir {} already contains a database; the light devnet generates a new genesis on \
             every start, so it needs an empty datadir",
            datadir.display()
        );
        Ok(())
    }

    /// Returns the chain the stack is running.
    pub const fn chain(&self) -> &LightChain {
        &self.chain
    }

    /// Returns the configured interval between locally produced blocks.
    pub const fn block_interval(&self) -> DevnetBlockInterval {
        self.block_interval
    }

    /// Returns the block gas limit of locally produced blocks.
    pub const fn block_gas_limit(&self) -> u64 {
        self.chain.block_gas_limit()
    }

    /// Returns the L2 chain ID.
    pub fn chain_id(&self) -> u64 {
        self.chain.chain_id()
    }

    /// Returns the genesis block hash.
    pub fn genesis_hash(&self) -> B256 {
        self.chain.chain_spec.genesis_hash()
    }

    /// Returns the builder RPC URL.
    pub fn builder_rpc_url(&self) -> Result<Url> {
        self.builder.rpc_url()
    }

    /// Returns the builder WebSocket RPC URL.
    pub fn builder_ws_url(&self) -> Result<Url> {
        self.builder.ws_url()
    }

    /// Returns the builder Prometheus metrics URL.
    pub fn builder_metrics_url(&self) -> Result<Url> {
        self.builder.metrics_url()
    }

    /// Returns the builder Flashblocks WebSocket URL.
    pub fn builder_flashblocks_url(&self) -> Result<Url> {
        Url::parse(&self.builder.flashblocks_url()).wrap_err("invalid builder Flashblocks URL")
    }

    /// Returns the builder datadir.
    pub fn datadir(&self) -> &Path {
        self.builder.datadir()
    }

    /// Waits for the standalone sequencer to report a fatal runtime error.
    pub async fn next_error(&mut self) -> String {
        self.standalone_consensus.next_error().await
    }

    /// Gracefully stops the sequencer before shutting down the execution node.
    pub async fn shutdown(self) -> Result<()> {
        self.standalone_consensus.shutdown().await;
        self.builder.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use alloy_consensus::BlockHeader;
    use base_common_chains::Upgrades;
    use base_common_consensus::JovianExtraData;
    use base_common_genesis::BaseUpgrade;
    use reth_ethereum_forks::ForkCondition;
    use tempfile::TempDir;

    use super::{LIGHT_STARTUP_LEAD, LightChain, LightL2Stack};
    use crate::DevnetBlockInterval;

    const GENESIS_TIMESTAMP: u64 = 2_000_000_000;

    fn generated(interval: DevnetBlockInterval) -> LightChain {
        LightChain::generated(GENESIS_TIMESTAMP, interval, LightChain::default_gas_limit(interval))
            .unwrap()
    }

    #[test]
    fn generated_execution_and_rollup_genesis_agree() {
        let chain = generated(DevnetBlockInterval::TwoSeconds);
        let header = chain.chain_spec.genesis_header();

        assert_eq!(chain.rollup_config.genesis.l2.hash, chain.chain_spec.genesis_hash());
        assert_eq!(chain.rollup_config.genesis.l2.number, header.number());
        assert_eq!(chain.rollup_config.genesis.l2_time, GENESIS_TIMESTAMP);
        assert_eq!(header.timestamp(), GENESIS_TIMESTAMP);
        assert_eq!(header.gas_limit(), chain.block_gas_limit());
        assert_eq!(chain.rollup_config.l2_chain_id.id(), chain.chain_id());
        assert_eq!(chain.l1_info.id(), chain.rollup_config.genesis.l1);
        assert_eq!(chain.l1_info.sequence_number(), 0);
    }

    #[test]
    fn generated_genesis_encodes_jovian_fee_parameters() {
        let chain = generated(DevnetBlockInterval::TwoSeconds);

        let (elasticity, denominator, min_base_fee) =
            JovianExtraData::decode(chain.chain_spec.genesis_header().extra_data()).unwrap();

        assert_eq!(elasticity, chain.system_config.eip1559_elasticity.unwrap());
        assert_eq!(denominator, chain.system_config.eip1559_denominator.unwrap());
        assert_eq!(Some(min_base_fee), chain.system_config.min_base_fee);
    }

    #[test]
    fn generated_chain_activates_forks_through_cobalt_at_genesis() {
        let chain = generated(DevnetBlockInterval::TwoSeconds);

        assert!(chain.chain_spec.is_jovian_active_at_timestamp(GENESIS_TIMESTAMP));
        assert!(chain.chain_spec.is_azul_active_at_timestamp(GENESIS_TIMESTAMP));
        assert!(chain.chain_spec.is_beryl_active_at_timestamp(GENESIS_TIMESTAMP));
        assert!(chain.chain_spec.is_cobalt_active_at_timestamp(GENESIS_TIMESTAMP));
        assert!(chain.rollup_config.is_jovian_active(GENESIS_TIMESTAMP));
        assert!(chain.rollup_config.is_cobalt_active(GENESIS_TIMESTAMP));
        assert!(chain.chain_spec.activation_admin_address.is_some());
    }

    #[test]
    fn two_second_chain_keeps_legacy_timing() {
        let chain = generated(DevnetBlockInterval::TwoSeconds);
        let first = chain.first_block_timestamp();

        assert_eq!(first, GENESIS_TIMESTAMP + 2);
        assert!(!chain.rollup_config.is_denim_active(first + 1_000));
        assert!(!chain.chain_spec.is_denim_active_at_timestamp(first + 1_000));
        assert_eq!(chain.rollup_config.l2_block_timestamp_parts(2), (GENESIS_TIMESTAMP + 4, 0));
    }

    #[test]
    fn subsecond_chain_activates_denim_at_first_block_on_both_layers() {
        let chain = generated(DevnetBlockInterval::TwoHundredMilliseconds);
        let first = chain.first_block_timestamp();

        assert!(!chain.rollup_config.is_denim_active(first - 1));
        assert!(chain.rollup_config.is_denim_active(first));
        assert!(!chain.chain_spec.is_denim_active_at_timestamp(first - 1));
        assert!(chain.chain_spec.is_denim_active_at_timestamp(first));
        assert_eq!(chain.rollup_config.l2_block_timestamp_parts(1), (first, 0));
        assert_eq!(chain.rollup_config.l2_block_timestamp_parts(2), (first, 200));
        assert_eq!(chain.rollup_config.l2_block_timestamp_parts(6), (first + 1, 0));
    }

    #[test]
    fn subsecond_defaults_scale_gas_and_fee_denominator_by_ten() {
        let two_second = generated(DevnetBlockInterval::TwoSeconds);
        let subsecond = generated(DevnetBlockInterval::TwoHundredMilliseconds);

        assert_eq!(two_second.block_gas_limit(), subsecond.block_gas_limit() * 10);
        assert_eq!(
            subsecond.system_config.eip1559_denominator.unwrap(),
            two_second.system_config.eip1559_denominator.unwrap() * 10
        );
    }

    #[test]
    fn custom_gas_limit_sets_genesis_and_system_config() {
        let chain =
            LightChain::generated(GENESIS_TIMESTAMP, DevnetBlockInterval::TwoSeconds, 123_000_000)
                .unwrap();

        assert_eq!(chain.block_gas_limit(), 123_000_000);
        assert_eq!(chain.chain_spec.genesis_header().gas_limit(), 123_000_000);
    }

    #[test]
    fn rejects_zero_gas_limit() {
        let error = LightChain::generated(GENESIS_TIMESTAMP, DevnetBlockInterval::TwoSeconds, 0)
            .unwrap_err();

        assert!(error.to_string().contains("gas limit"));
    }

    #[test]
    fn generated_chain_includes_prefunded_anvil_accounts_and_base_time() {
        let chain = generated(DevnetBlockInterval::TwoSeconds);
        let alloc = &chain.chain_spec.genesis().alloc;

        assert!(alloc.contains_key(&crate::ANVIL_ACCOUNT_0.address));
        assert!(alloc.contains_key(&crate::ANVIL_ACCOUNT_9.address));
        assert!(alloc.contains_key(&base_common_consensus::Predeploys::BASE_TIME));
        assert_eq!(chain.chain_spec.fork(BaseUpgrade::Jovian), ForkCondition::Timestamp(0));
    }

    #[test]
    fn rejects_malformed_genesis_json() {
        let error = LightChain::from_json(
            b"{",
            b"{}",
            DevnetBlockInterval::TwoSeconds,
            UNIX_EPOCH + Duration::from_secs(GENESIS_TIMESTAMP),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("genesis"));
    }

    #[test]
    fn genesis_timestamp_uses_startup_lead() {
        let now = UNIX_EPOCH + Duration::from_secs(GENESIS_TIMESTAMP);

        assert_eq!(
            LightL2Stack::genesis_timestamp(now).unwrap(),
            GENESIS_TIMESTAMP + LIGHT_STARTUP_LEAD.as_secs()
        );
    }

    #[test]
    fn rejects_datadir_with_existing_database() {
        let datadir = TempDir::new().unwrap();
        LightL2Stack::ensure_fresh_datadir(datadir.path()).unwrap();
        std::fs::create_dir_all(datadir.path().join("db")).unwrap();
        std::fs::write(datadir.path().join("db/mdbx.dat"), []).unwrap();

        let error = LightL2Stack::ensure_fresh_datadir(datadir.path()).unwrap_err();

        assert!(error.to_string().contains("already contains a database"));
    }
}
