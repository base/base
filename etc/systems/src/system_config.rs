//! Stable configuration for system test container names and ports.

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256};
use base_common_chains::ChainConfig;
use base_common_genesis::RollupConfig;
use base_execution_chainspec::BaseChainSpec;
use clap::ValueEnum;
use eyre::{Result, WrapErr, bail, ensure};
use serde::{Deserialize, Serialize};

const DEFAULT_SLOT_DURATION: u64 = 1;

/// L1 implementation used by a devnet stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevnetL1Mode {
    /// Run the local Reth and Lighthouse L1 stack.
    Real,
    /// Do not run L1 services. Only valid for standalone snapshot continuation.
    None,
    /// Run deterministic execution RPC and beacon services.
    Fake,
}

/// Expected head of a snapshot-backed L2 execution database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevnetSnapshotHead {
    /// L2 block number at the snapshot boundary.
    pub number: u64,
    /// L2 block hash at the snapshot boundary.
    pub hash: B256,
    /// L2 block timestamp at the snapshot boundary.
    pub timestamp: u64,
}

/// One-time local account funding for a snapshot-backed development network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevnetPrefund {
    /// Account receiving minted ETH in the first local descendant block.
    pub address: Address,
    /// Amount of wei minted to the account.
    pub amount: u128,
}

/// Block interval used while locally extending an execution snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum DevnetBlockInterval {
    /// Produce one block every two seconds.
    #[default]
    #[value(name = "2s")]
    TwoSeconds,
    /// Produce one block every 200 milliseconds using `BaseTime` metadata.
    #[value(name = "200ms")]
    TwoHundredMilliseconds,
}

impl DevnetBlockInterval {
    /// Returns the wall-clock duration between blocks.
    pub const fn duration(self) -> Duration {
        match self {
            Self::TwoSeconds => Duration::from_secs(2),
            Self::TwoHundredMilliseconds => Duration::from_millis(200),
        }
    }
}

/// Chain inputs used to continue a Base snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotChainConfig {
    /// Built-in Base chain name or path to a Base genesis JSON file.
    pub chain: String,
    /// Rollup configuration JSON required for a custom genesis whose chain ID is not built in.
    pub rollup_config: Option<PathBuf>,
}

impl Default for SnapshotChainConfig {
    fn default() -> Self {
        Self { chain: "mainnet".to_string(), rollup_config: None }
    }
}

/// Fully resolved chain configuration used by a snapshot stack.
#[derive(Debug, Clone)]
pub struct ResolvedSnapshotChain {
    /// Execution chain specification.
    pub chain_spec: Arc<BaseChainSpec>,
    /// Rollup configuration used to validate and continue the snapshot.
    pub rollup_config: Arc<RollupConfig>,
    /// L1 chain ID.
    pub l1_chain_id: u64,
    /// L2 chain ID.
    pub l2_chain_id: u64,
}

impl SnapshotChainConfig {
    /// Resolves a built-in chain name or a Base genesis JSON plus its rollup configuration.
    pub fn resolve(&self) -> Result<ResolvedSnapshotChain> {
        if let Some(config) =
            ChainConfig::from_base_chain(&self.chain).or_else(|| ChainConfig::by_name(&self.chain))
        {
            return Ok(ResolvedSnapshotChain {
                chain_spec: Arc::new(BaseChainSpec::try_from(config)?),
                rollup_config: Arc::new(config.rollup_config()),
                l1_chain_id: config.l1_chain_id,
                l2_chain_id: config.chain_id,
            });
        }

        let path = Path::new(&self.chain);
        let contents = std::fs::read(path)
            .wrap_err_with(|| format!("failed to read snapshot chain JSON {}", path.display()))?;
        let genesis: Genesis = serde_json::from_slice(&contents)
            .wrap_err_with(|| format!("failed to parse snapshot chain JSON {}", path.display()))?;
        let chain_spec = BaseChainSpec::try_from_genesis(genesis)
            .wrap_err_with(|| format!("invalid Base genesis JSON {}", path.display()))?;
        let l2_chain_id = chain_spec.chain.id();
        let rollup_config = if let Some(config) = ChainConfig::by_chain_id(l2_chain_id) {
            config.rollup_config()
        } else {
            let rollup_path = self.rollup_config.as_ref().ok_or_else(|| {
                eyre::eyre!(
                    "custom snapshot chain JSON {} requires --rollup-config",
                    path.display()
                )
            })?;
            let contents = std::fs::read(rollup_path).wrap_err_with(|| {
                format!("failed to read snapshot rollup config {}", rollup_path.display())
            })?;
            let config: RollupConfig = serde_json::from_slice(&contents).wrap_err_with(|| {
                format!("failed to parse snapshot rollup config {}", rollup_path.display())
            })?;
            ensure!(
                config.l2_chain_id.id() == l2_chain_id,
                "rollup config L2 chain ID {} does not match chain JSON ID {l2_chain_id}",
                config.l2_chain_id.id()
            );
            config
        };

        Ok(ResolvedSnapshotChain {
            chain_spec: Arc::new(chain_spec),
            l1_chain_id: rollup_config.l1_chain_id,
            l2_chain_id,
            rollup_config: Arc::new(rollup_config),
        })
    }
}

/// Writable execution datadirs used to continue a Base snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevnetSnapshotConfig {
    /// Built-in chain name or custom chain JSON plus optional rollup configuration.
    pub chain: SnapshotChainConfig,
    /// Writable snapshot datadir used by the builder.
    pub builder_datadir: PathBuf,
    /// Writable snapshot datadir used by the client.
    pub client_datadir: PathBuf,
    /// Expected L2 chain ID stored in both datadirs.
    pub expected_chain_id: u64,
    /// Optional expected snapshot head. The launcher discovers it when omitted.
    pub expected_head: Option<DevnetSnapshotHead>,
    /// Optional account funding applied only to the first local descendant.
    pub prefund: Option<DevnetPrefund>,
    /// Block interval for locally produced descendants.
    #[serde(default)]
    pub block_interval: DevnetBlockInterval,
}

/// Initial execution state used by a devnet stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevnetL2State {
    /// Generate a fresh local L2 genesis and empty execution databases.
    Fresh,
    /// Continue from caller-owned writable Base snapshot datadirs.
    Snapshot(Box<DevnetSnapshotConfig>),
}

/// Stable port assignments for system test components.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemTestPorts {
    /// L1 HTTP RPC port
    pub l1_http: u16,
    /// L1 WebSocket port
    pub l1_ws: u16,
    /// L1 Auth RPC port
    pub l1_auth: u16,
    /// L1 P2P port
    pub l1_p2p: u16,
    /// L1 CL HTTP port
    pub l1_cl_http: u16,
    /// L1 CL P2P port
    pub l1_cl_p2p: u16,

    /// L2 Execution bootnode P2P port
    pub l2_el_bootnode_p2p: u16,
    /// L2 Consensus bootnode P2P port
    pub l2_cl_bootnode_p2p: u16,

    /// L2 Builder HTTP RPC port
    pub l2_builder_http: u16,
    /// L2 Builder WebSocket port
    pub l2_builder_ws: u16,
    /// L2 Builder Auth RPC port
    pub l2_builder_auth: u16,
    /// L2 Builder P2P port
    pub l2_builder_p2p: u16,
    /// L2 Builder Flashblocks port
    pub l2_builder_flashblocks: u16,
    /// L2 Builder Metrics port
    pub l2_builder_metrics: u16,
    /// L2 Builder CL RPC port
    pub l2_builder_cl_rpc: u16,
    /// L2 Builder CL P2P port
    pub l2_builder_cl_p2p: u16,
    /// L2 Builder CL Metrics port
    pub l2_builder_cl_metrics: u16,

    /// L2 Client HTTP RPC port
    pub l2_client_http: u16,
    /// L2 Client WebSocket port
    pub l2_client_ws: u16,
    /// L2 Client Auth RPC port
    pub l2_client_auth: u16,
    /// L2 Client P2P port
    pub l2_client_p2p: u16,
    /// L2 Client Metrics port
    pub l2_client_metrics: u16,
    /// L2 Client CL RPC port
    pub l2_client_cl_rpc: u16,
    /// L2 Client CL P2P port
    pub l2_client_cl_p2p: u16,
    /// L2 Client CL Metrics port
    pub l2_client_cl_metrics: u16,
}

impl SystemTestPorts {
    /// Returns the standard system test port assignments.
    pub const fn standard() -> Self {
        Self {
            l1_http: 4545,
            l1_ws: 4546,
            l1_auth: 4551,
            l1_p2p: 4303,
            l1_cl_http: 4052,
            l1_cl_p2p: 4900,

            l2_el_bootnode_p2p: 9303,
            l2_cl_bootnode_p2p: 9003,

            l2_builder_http: 7545,
            l2_builder_ws: 7546,
            l2_builder_auth: 7551,
            l2_builder_p2p: 7303,
            l2_builder_flashblocks: 7111,
            l2_builder_metrics: 7090,
            l2_builder_cl_rpc: 7549,
            l2_builder_cl_p2p: 7003,
            l2_builder_cl_metrics: 7300,

            l2_client_http: 8545,
            l2_client_ws: 8546,
            l2_client_auth: 8551,
            l2_client_p2p: 8303,
            l2_client_metrics: 8090,
            l2_client_cl_rpc: 8549,
            l2_client_cl_p2p: 8003,
            l2_client_cl_metrics: 8300,
        }
    }
}

/// Complete stable configuration for system tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StableSystemTestConfig {
    /// Docker network name
    pub network_name: String,
    /// Port assignments
    pub ports: SystemTestPorts,
}

impl StableSystemTestConfig {
    /// Returns the standard system test configuration.
    pub fn standard() -> Self {
        Self { network_name: crate::network_name().to_string(), ports: SystemTestPorts::standard() }
    }
}

/// Canonical configuration shared by developer devnets and programmatic system tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevnetConfig {
    /// L1 chain ID.
    pub l1_chain_id: u64,
    /// L2 chain ID.
    pub l2_chain_id: u64,
    /// L1 slot duration in seconds.
    pub l1_slot_duration: u64,
    /// L1 implementation to launch.
    pub l1_mode: DevnetL1Mode,
    /// Initial L2 execution state.
    pub l2_state: DevnetL2State,
    /// Stable network and port values shared with the Compose devnet.
    pub stable: StableSystemTestConfig,
    /// Bind the stable ports instead of dynamically allocated test ports.
    pub use_stable_ports: bool,
}

impl DevnetConfig {
    /// Returns the standard fresh local devnet configuration.
    pub fn standard() -> Self {
        Self {
            l1_chain_id: 1337,
            l2_chain_id: 84_538_453,
            l1_slot_duration: DEFAULT_SLOT_DURATION,
            l1_mode: DevnetL1Mode::Real,
            l2_state: DevnetL2State::Fresh,
            stable: StableSystemTestConfig::standard(),
            use_stable_ports: false,
        }
    }

    /// Returns an L1-free Base snapshot configuration for a resolved chain input.
    pub fn snapshot(
        builder_datadir: PathBuf,
        client_datadir: PathBuf,
        chain: SnapshotChainConfig,
    ) -> Result<Self> {
        let resolved = chain.resolve()?;
        Ok(Self {
            l1_chain_id: resolved.l1_chain_id,
            l2_chain_id: resolved.l2_chain_id,
            l1_slot_duration: DEFAULT_SLOT_DURATION,
            l1_mode: DevnetL1Mode::None,
            l2_state: DevnetL2State::Snapshot(Box::new(DevnetSnapshotConfig {
                chain,
                builder_datadir,
                client_datadir,
                expected_chain_id: resolved.l2_chain_id,
                expected_head: None,
                prefund: None,
                block_interval: DevnetBlockInterval::default(),
            })),
            stable: StableSystemTestConfig::standard(),
            use_stable_ports: false,
        })
    }

    /// Validates mode combinations and snapshot identity expectations.
    pub fn validate(&self) -> Result<()> {
        ensure!(self.l1_slot_duration > 0, "L1 slot duration must be greater than zero");

        let DevnetL2State::Snapshot(snapshot) = &self.l2_state else { return Ok(()) };

        if self.l1_mode != DevnetL1Mode::None {
            bail!("snapshot-backed L2 state requires L1-free mode")
        }
        ensure!(
            snapshot.expected_chain_id == self.l2_chain_id,
            "snapshot chain ID {} does not match configured L2 chain ID {}",
            snapshot.expected_chain_id,
            self.l2_chain_id
        );
        ensure!(
            snapshot.builder_datadir != snapshot.client_datadir,
            "builder and client snapshot datadirs must be distinct"
        );
        ensure!(snapshot.builder_datadir.is_dir(), "builder snapshot datadir does not exist");
        ensure!(snapshot.client_datadir.is_dir(), "client snapshot datadir does not exist");

        let builder_datadir = std::fs::canonicalize(&snapshot.builder_datadir)
            .wrap_err("Failed to resolve builder snapshot datadir")?;
        let client_datadir = std::fs::canonicalize(&snapshot.client_datadir)
            .wrap_err("Failed to resolve client snapshot datadir")?;
        ensure!(
            builder_datadir != client_datadir,
            "builder and client snapshot datadirs resolve to the same directory"
        );

        #[cfg(unix)]
        {
            let builder_metadata = std::fs::metadata(builder_datadir)?;
            let client_metadata = std::fs::metadata(client_datadir)?;
            ensure!(
                (builder_metadata.dev(), builder_metadata.ino())
                    != (client_metadata.dev(), client_metadata.ino()),
                "builder and client snapshot datadirs reference the same directory"
            );
        }

        Ok(())
    }
}

impl Default for DevnetConfig {
    fn default() -> Self {
        Self::standard()
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use alloy_genesis::Genesis;
    use base_common_genesis::RollupConfig;
    use tempfile::TempDir;

    use super::{
        DEFAULT_SLOT_DURATION, DevnetConfig, DevnetL1Mode, DevnetL2State, SnapshotChainConfig,
    };

    #[test]
    fn standard_config_matches_developer_devnet() {
        let config = DevnetConfig::standard();
        let developer_env = include_str!("../../docker/devnet-env");

        assert_eq!(config.l1_chain_id, 1337);
        assert_eq!(config.l2_chain_id, 84_538_453);
        assert_eq!(config.l1_slot_duration, DEFAULT_SLOT_DURATION);
        assert_eq!(config.l1_mode, DevnetL1Mode::Real);
        assert_eq!(config.l2_state, DevnetL2State::Fresh);
        assert_eq!(config.stable.ports.l1_http, 4545);
        assert_eq!(config.stable.ports.l2_builder_http, 7545);
        assert_eq!(config.stable.ports.l2_client_http, 8545);
        assert!(!config.use_stable_ports);
        assert!(developer_env.contains("L1_CHAIN_ID=1337\n"));
        assert!(developer_env.contains("L2_CHAIN_ID=84538453\n"));
        assert!(developer_env.contains("L1_HTTP_PORT=4545\n"));
        assert!(developer_env.contains("L2_BUILDER_HTTP_PORT=7545\n"));
        assert!(developer_env.contains("L2_CLIENT_HTTP_PORT=8545\n"));
        config.validate().expect("standard devnet config should be valid");
    }

    #[test]
    fn snapshot_is_l1_free() {
        let parent = TempDir::new().unwrap();
        let builder = parent.path().join("builder");
        let client = parent.path().join("client");
        std::fs::create_dir_all(&builder).unwrap();
        std::fs::create_dir_all(&client).unwrap();
        let config =
            DevnetConfig::snapshot(builder, client, SnapshotChainConfig::default()).unwrap();

        assert_eq!(config.l1_chain_id, 1);
        assert_eq!(config.l2_chain_id, 8453);
        assert_eq!(config.l1_mode, DevnetL1Mode::None);
        config.validate().expect("Base mainnet snapshot config should be valid");
    }

    #[test]
    fn resolves_builtin_sepolia_snapshot_chain() {
        let chain = SnapshotChainConfig { chain: "sepolia".to_string(), rollup_config: None }
            .resolve()
            .expect("Base Sepolia should resolve as a built-in snapshot chain");

        assert_eq!(chain.l1_chain_id, 11_155_111);
        assert_eq!(chain.l2_chain_id, 84_532);
        assert_eq!(chain.rollup_config.l2_chain_id.id(), 84_532);
        assert_eq!(chain.chain_spec.chain.id(), 84_532);
    }

    #[test]
    fn resolves_custom_genesis_with_rollup_config() {
        const CHAIN_ID: u64 = 123_456;
        let directory = TempDir::new().unwrap();
        let chain_path = directory.path().join("genesis.json");
        let rollup_path = directory.path().join("rollup.json");
        let mut genesis = Genesis::default();
        genesis.config.chain_id = CHAIN_ID;
        std::fs::write(&chain_path, serde_json::to_vec(&genesis).unwrap()).unwrap();
        let rollup = RollupConfig {
            l1_chain_id: 11_155_111,
            l2_chain_id: CHAIN_ID.into(),
            ..Default::default()
        };
        std::fs::write(&rollup_path, serde_json::to_vec(&rollup).unwrap()).unwrap();

        let chain = SnapshotChainConfig {
            chain: chain_path.to_string_lossy().into_owned(),
            rollup_config: Some(rollup_path),
        }
        .resolve()
        .expect("custom genesis and matching rollup config should resolve");

        assert_eq!(chain.l1_chain_id, 11_155_111);
        assert_eq!(chain.l2_chain_id, CHAIN_ID);
        assert_eq!(chain.rollup_config.l2_chain_id.id(), CHAIN_ID);
        assert_eq!(chain.chain_spec.chain.id(), CHAIN_ID);
    }

    #[test]
    fn snapshot_datadirs_must_be_distinct() {
        let datadir = TempDir::new().unwrap();
        let datadir = datadir.path().to_path_buf();
        let config =
            DevnetConfig::snapshot(datadir.clone(), datadir, SnapshotChainConfig::default())
                .unwrap();

        let error = config.validate().expect_err("shared snapshot datadir should be rejected");
        assert!(error.to_string().contains("must be distinct"));
    }

    #[test]
    #[cfg(unix)]
    fn snapshot_datadirs_must_not_alias() {
        let datadir = TempDir::new().unwrap();
        let alias_parent = TempDir::new().unwrap();
        let alias = alias_parent.path().join("alias");
        symlink(datadir.path(), &alias).unwrap();
        let config = DevnetConfig::snapshot(
            datadir.path().to_path_buf(),
            alias,
            SnapshotChainConfig::default(),
        )
        .unwrap();

        let error = config.validate().expect_err("aliased snapshot datadir should be rejected");
        assert!(error.to_string().contains("resolve to the same directory"));
    }
}
