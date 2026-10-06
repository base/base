//! Command-line launcher for development networks.

use std::{num::NonZeroU64, path::PathBuf, sync::Arc, time::Duration};

use alloy_primitives::{Address, B256};
use base_common_chains::ChainConfig;
use base_common_genesis::RollupConfig;
use clap::{Args, Parser, Subcommand};
use eyre::{Result, WrapErr, ensure, eyre};
use serde::Serialize;
use tracing::debug;
use url::Url;

use crate::{
    DevnetBlockInterval, DevnetConfig, DevnetL2State, DevnetPrefund, DevnetSnapshotHead,
    ResolvedSnapshotChain, SharedL1, SnapshotChainConfig, SnapshotForkFinder, SnapshotForkSource,
    SnapshotInspection, SnapshotL2Stack, SystemTestStackBuilder,
};

/// Local Base development network launcher.
#[derive(Debug, Parser)]
#[command(author, version, about = "Run a local Base development network")]
pub struct DevnetCli {
    /// Development network mode.
    #[command(subcommand)]
    pub command: DevnetCommand,
}

/// Supported development network modes.
#[derive(Debug, Subcommand)]
pub enum DevnetCommand {
    /// Continue Base snapshot datadirs without an L1.
    Snapshot(SnapshotArgs),
    /// Start a CI-scoped shared L1 and write its runtime manifest.
    SharedL1(SharedL1Args),
    /// Print a snapshot source node's validated latest, safe, and finalized heads, and optionally
    /// the L1 fork block that derives its unsafe tail, as JSON.
    InspectSnapshot(InspectSnapshotArgs),
}

/// Arguments for read-only inspection of a snapshot source node over RPC.
#[derive(Debug, Args)]
pub struct InspectSnapshotArgs {
    /// Execution JSON-RPC URL of the snapshot source node.
    // Parsed in `run` because clap echoes rejected values, which can carry credentials.
    #[arg(long)]
    pub rpc_url: String,
    /// Built-in Base chain name or path to a Base genesis JSON file.
    #[arg(long, default_value = "mainnet")]
    pub chain: String,
    /// Effective rollup config, including locally scheduled upgrades, for decoding the heads.
    /// Its chain and genesis identity must match the selected chain.
    #[arg(long)]
    pub rollup_config: Option<PathBuf>,
    /// Also report as `fork` the canonical finalized L1 block that derives the unsafe tail.
    /// Reads upstream L1 URLs only from `SNAPSHOT_UPSTREAM_EXECUTION` and
    /// `SNAPSHOT_UPSTREAM_BEACON`.
    #[arg(long)]
    pub find_fork: bool,
    /// Deadline in seconds for the whole fork discovery.
    #[arg(long, default_value_t = 600)]
    pub timeout: u64,
}

/// Arguments for a CI-scoped shared L1 fixture.
#[derive(Debug, Args)]
pub struct SharedL1Args {
    /// File written after the shared L1 is ready for consumers.
    #[arg(long)]
    pub runtime_file: PathBuf,
    /// Docker network shared with live L2 deployments.
    #[arg(long)]
    pub network_name: String,
}

/// Arguments for an L1-free Base snapshot network.
#[derive(Debug, Args)]
pub struct SnapshotArgs {
    /// Built-in Base chain name or path to a Base genesis JSON file.
    #[arg(long, default_value = "mainnet")]
    pub chain: String,
    /// Rollup config JSON for a custom chain JSON whose chain ID is not built in.
    #[arg(long)]
    pub rollup_config: Option<PathBuf>,
    /// Writable builder snapshot datadir.
    #[arg(long, env = "BASE_SNAPSHOT_BUILDER_DATADIR")]
    pub builder_datadir: PathBuf,
    /// Writable client snapshot datadir for the same chain.
    #[arg(long, env = "BASE_SNAPSHOT_CLIENT_DATADIR")]
    pub client_datadir: PathBuf,
    /// Bind the stable developer ports instead of allocating free ports.
    #[arg(long)]
    pub stable_ports: bool,
    /// Interval between locally produced blocks.
    #[arg(long, value_enum, default_value_t)]
    pub block_interval: DevnetBlockInterval,
    /// Block gas limit for locally produced descendants. Defaults to 10 Ggas for 2s blocks and
    /// 1 Ggas for 200ms blocks.
    #[arg(long)]
    pub block_gas_limit: Option<NonZeroU64>,
    /// Account to mint ETH to in the first local descendant block.
    #[arg(long)]
    pub prefund_address: Option<Address>,
    /// Amount of wei minted to `--prefund-address`.
    #[arg(long, default_value_t = 1_000_000_000_000_000_000_000_u128)]
    pub prefund_amount: u128,
    /// Expected snapshot boundary block number.
    #[arg(long, requires_all = ["expected_head_hash", "expected_head_timestamp"])]
    pub expected_head_number: Option<u64>,
    /// Expected snapshot boundary block hash.
    #[arg(long, requires_all = ["expected_head_number", "expected_head_timestamp"])]
    pub expected_head_hash: Option<B256>,
    /// Expected snapshot boundary Unix timestamp.
    #[arg(long, requires_all = ["expected_head_number", "expected_head_hash"])]
    pub expected_head_timestamp: Option<u64>,
    /// Machine-readable endpoint and boundary output.
    #[arg(long, default_value = "runtime.json")]
    pub runtime_file: PathBuf,
}

/// Machine-readable state emitted by the snapshot devnet launcher.
#[derive(Debug, Serialize)]
pub struct SnapshotRuntime {
    /// Current launcher state.
    pub status: &'static str,
    /// L2 chain ID.
    pub chain_id: u64,
    /// Snapshot boundary block number.
    pub boundary_number: u64,
    /// Snapshot boundary block hash.
    pub boundary_hash: B256,
    /// Configured interval between local blocks, in milliseconds.
    pub block_interval_ms: u64,
    /// Configured block gas limit for local descendants.
    pub block_gas_limit: u64,
    /// Builder execution JSON-RPC URL.
    pub builder_rpc_url: String,
    /// Builder Flashblocks WebSocket URL.
    pub builder_flashblocks_url: String,
    /// Client execution JSON-RPC URL.
    pub client_rpc_url: String,
}

impl DevnetCli {
    /// Runs the selected development network until interrupted.
    pub async fn run(self) -> Result<()> {
        match self.command {
            DevnetCommand::Snapshot(args) => args.run().await,
            DevnetCommand::SharedL1(args) => args.run().await,
            DevnetCommand::InspectSnapshot(args) => args.run().await,
        }
    }
}

impl SharedL1Args {
    /// Starts the fixture, publishes its manifest, and waits for shutdown.
    pub async fn run(self) -> Result<()> {
        let stack = SharedL1::start(self.network_name).await?;
        stack.runtime().write(&self.runtime_file)?;
        println!("shared L1 ready: {}", self.runtime_file.display());
        tokio::signal::ctrl_c().await.wrap_err("failed to listen for Ctrl-C")?;
        stack.shutdown().await
    }
}

impl InspectSnapshotArgs {
    /// Resolves the chain and applies an explicit inspection schedule without changing identity.
    pub fn resolved_chain(&self) -> Result<ResolvedSnapshotChain> {
        let mut chain = SnapshotChainConfig {
            chain: self.chain.clone(),
            rollup_config: self.rollup_config.clone(),
        }
        .resolve()?;
        if let Some(path) = &self.rollup_config {
            let contents = std::fs::read(path).wrap_err_with(|| {
                format!("failed to read snapshot rollup config {}", path.display())
            })?;
            let config: RollupConfig = serde_json::from_slice(&contents).wrap_err_with(|| {
                format!("failed to parse snapshot rollup config {}", path.display())
            })?;
            ensure!(
                config.l2_chain_id.id() == chain.l2_chain_id
                    && config.l1_chain_id == chain.l1_chain_id
                    && config.genesis == chain.rollup_config.genesis,
                "inspection config changes chain or genesis identity"
            );
            chain.rollup_config = Arc::new(config);
        }
        // Built-in configs can use placeholder genesis values, but an explicit chain JSON
        // supplies a header against which its effective rollup genesis can be checked.
        if ChainConfig::by_any_name(&self.chain).is_none() {
            let genesis = &chain.chain_spec.genesis_header;
            let configured = chain.rollup_config.genesis;
            ensure!(
                configured.l2.hash == genesis.hash()
                    && configured.l2.number == genesis.number
                    && configured.l2_time == genesis.timestamp,
                "inspection config changes chain or genesis identity"
            );
        }
        Ok(chain)
    }

    /// Prints exactly one JSON object describing the node's labeled heads, plus `fork` when
    /// requested, to stdout.
    pub async fn run(self) -> Result<()> {
        let inspection = async {
            let rpc_url: Url =
                self.rpc_url.parse().map_err(|error| eyre!("invalid --rpc-url: {error}"))?;
            let chain = self.resolved_chain()?;
            let source = self.find_fork.then(SnapshotForkSource::from_env).transpose()?;
            let mut inspection =
                SnapshotInspection::read(rpc_url.clone(), chain.rollup_config, chain.l2_chain_id)
                    .await?;
            if let Some(source) = source {
                let finder = SnapshotForkFinder {
                    rpc_url,
                    source,
                    timeout: Duration::from_secs(self.timeout),
                };
                inspection.fork = Some(finder.find(&inspection).await?);
            }
            Ok::<_, eyre::Report>(inspection)
        }
        .await
        .map_err(|error| {
            // Source errors can quote the RPC URL, including credentials, raw response bodies, or
            // config file contents. Return only the inspector's own top-level reason; the full
            // chain stays behind opt-in logs.
            debug!(error = ?error, "snapshot inspection failed");
            eyre!("{error}")
        })?;
        println!("{}", serde_json::to_string(&inspection)?);
        Ok(())
    }
}

impl SnapshotArgs {
    /// Starts a snapshot-backed stack, writes its runtime manifest, and waits for shutdown.
    pub async fn run(self) -> Result<()> {
        let expected_head = match (
            self.expected_head_number,
            self.expected_head_hash,
            self.expected_head_timestamp,
        ) {
            (Some(number), Some(hash), Some(timestamp)) => {
                Some(DevnetSnapshotHead { number, hash, timestamp })
            }
            (None, None, None) => None,
            _ => unreachable!("clap requires all expected-head fields together"),
        };
        let mut config = DevnetConfig::snapshot(
            self.builder_datadir,
            self.client_datadir,
            SnapshotChainConfig { chain: self.chain, rollup_config: self.rollup_config },
        )?;
        config.use_stable_ports = self.stable_ports;
        let DevnetL2State::Snapshot(snapshot) = &mut config.l2_state else {
            unreachable!("snapshot constructor must create snapshot state")
        };
        snapshot.expected_head = expected_head;
        snapshot.block_interval = self.block_interval;
        snapshot.block_gas_limit = self.block_gas_limit.map(NonZeroU64::get);
        snapshot.prefund = self
            .prefund_address
            .map(|address| DevnetPrefund { address, amount: self.prefund_amount });
        config.validate()?;

        let stack =
            SystemTestStackBuilder::new().with_devnet_config(config).build_snapshot().await?;
        let runtime = SnapshotRuntime::ready(&stack)?;
        let encoded = serde_json::to_vec_pretty(&runtime)?;
        std::fs::write(&self.runtime_file, encoded).wrap_err_with(|| {
            format!("failed to write runtime manifest {}", self.runtime_file.display())
        })?;

        println!("snapshot devnet ready");
        println!("builder RPC: {}", runtime.builder_rpc_url);
        println!("client RPC:  {}", runtime.client_rpc_url);
        println!("block gas:   {}", runtime.block_gas_limit);
        println!("runtime:     {}", self.runtime_file.display());
        println!("press Ctrl-C to stop");
        tokio::signal::ctrl_c().await.wrap_err("failed to listen for Ctrl-C")?;
        stack.shutdown().await?;
        Ok(())
    }
}

impl SnapshotRuntime {
    /// Captures the ready endpoints and immutable boundary from a running stack.
    pub fn ready(stack: &SnapshotL2Stack) -> Result<Self> {
        let boundary = stack.boundary();
        Ok(Self {
            status: "ready",
            chain_id: stack.chain_id(),
            boundary_number: boundary.head.number,
            boundary_hash: boundary.head.hash,
            block_interval_ms: stack.block_interval().duration().as_millis() as u64,
            block_gas_limit: stack.block_gas_limit(),
            builder_rpc_url: stack.builder_rpc_url()?.to_string(),
            builder_flashblocks_url: stack.builder_flashblocks_url()?.to_string(),
            client_rpc_url: stack.client_rpc_url()?.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, num::NonZeroU64};

    use alloy_genesis::Genesis;
    use base_common_genesis::RollupConfig;
    use base_execution_chainspec::BaseChainSpec;
    use clap::Parser;
    use serde_json::json;

    use super::{DevnetCli, DevnetCommand, InspectSnapshotArgs};
    use crate::{DevnetBlockInterval, SnapshotChainConfig, test_utils::SnapshotRpcFixture};

    #[test]
    fn parses_snapshot_command() {
        let cli = DevnetCli::try_parse_from([
            "base-devnet",
            "snapshot",
            "--chain",
            "sepolia",
            "--builder-datadir",
            "/tmp/builder",
            "--client-datadir",
            "/tmp/client",
            "--prefund-address",
            "0x0000000000000000000000000000000000000001",
            "--block-interval",
            "200ms",
        ])
        .unwrap();

        let DevnetCommand::Snapshot(args) = cli.command else {
            panic!("expected snapshot command")
        };
        assert_eq!(args.chain, "sepolia");
        assert_eq!(args.builder_datadir.to_str(), Some("/tmp/builder"));
        assert!(args.prefund_address.is_some());
        assert_eq!(args.block_interval, DevnetBlockInterval::TwoHundredMilliseconds);
        assert!(args.block_gas_limit.is_none());
    }

    #[test]
    fn parses_snapshot_block_gas_limit() {
        let cli = DevnetCli::try_parse_from([
            "base-devnet",
            "snapshot",
            "--builder-datadir",
            "/tmp/builder",
            "--client-datadir",
            "/tmp/client",
            "--block-gas-limit",
            "12000000000",
        ])
        .unwrap();

        let DevnetCommand::Snapshot(args) = cli.command else {
            panic!("expected snapshot command")
        };
        assert_eq!(args.block_gas_limit.map(NonZeroU64::get), Some(12_000_000_000));
    }

    #[test]
    fn parses_inspect_snapshot_command() {
        let cli = DevnetCli::try_parse_from([
            "base-devnet",
            "inspect-snapshot",
            "--rpc-url",
            "http://127.0.0.1:8545",
        ])
        .unwrap();

        let DevnetCommand::InspectSnapshot(args) = cli.command else {
            panic!("expected inspect-snapshot command")
        };
        assert_eq!(args.rpc_url, "http://127.0.0.1:8545");
        assert_eq!(args.chain, "mainnet");
        assert!(args.rollup_config.is_none());
        assert!(!args.find_fork);
        assert_eq!(args.timeout, 600);

        let cli = DevnetCli::try_parse_from([
            "base-devnet",
            "inspect-snapshot",
            "--rpc-url",
            "http://node:8545",
            "--chain",
            "/tmp/genesis.json",
            "--rollup-config",
            "/tmp/rollup.json",
            "--find-fork",
            "--timeout",
            "90",
        ])
        .unwrap();
        let DevnetCommand::InspectSnapshot(args) = cli.command else {
            panic!("expected inspect-snapshot command")
        };
        assert_eq!(args.chain, "/tmp/genesis.json");
        assert_eq!(args.rollup_config.unwrap().to_str(), Some("/tmp/rollup.json"));
        assert!(args.find_fork);
        assert_eq!(args.timeout, 90);
    }

    #[test]
    fn inspection_uses_local_schedule_without_changing_identity() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let args = InspectSnapshotArgs {
            rpc_url: "http://127.0.0.1:8545".parse().unwrap(),
            chain: "mainnet".to_string(),
            rollup_config: None,
            find_fork: false,
            timeout: 600,
        };
        let mut config = (*args.resolved_chain().unwrap().rollup_config).clone();
        config.upgrades.base.denim = Some(2_000_000_000);
        std::fs::write(file.path(), serde_json::to_vec(&config).unwrap()).unwrap();
        let args = InspectSnapshotArgs { rollup_config: Some(file.path().into()), ..args };
        assert_eq!(
            args.resolved_chain().unwrap().rollup_config.upgrades.base.denim,
            Some(2_000_000_000)
        );

        let identity_changes: [fn(&mut RollupConfig); 3] = [
            |config| config.l2_chain_id = 1.into(),
            |config| config.l1_chain_id += 1,
            |config| config.genesis.l2_time += 1,
        ];
        for change in identity_changes {
            let mut changed = config.clone();
            change(&mut changed);
            std::fs::write(file.path(), serde_json::to_vec(&changed).unwrap()).unwrap();
            let error = args.resolved_chain().unwrap_err();
            assert!(error.to_string().contains("changes chain or genesis identity"), "{error:?}");
        }
    }

    #[tokio::test]
    async fn inspection_errors_do_not_expose_rpc_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let unreachable =
            format!("http://secret-user:secret-password@{address}/secret-path?token=secret-query");
        // Transport errors quote the request URL, and decode errors quote the raw response body.
        let mut fixture = SnapshotRpcFixture::default();
        fixture.blocks.insert("finalized".into(), json!({ "number": "secret-response" }));
        let (served, handle) = fixture.serve(SnapshotRpcFixture::CHAIN_ID).await;

        for (rpc_url, message) in [
            (unreachable, "failed to read snapshot chain ID"),
            (served.to_string(), "failed to read snapshot finalized block"),
        ] {
            let args = InspectSnapshotArgs {
                rpc_url,
                chain: "mainnet".into(),
                rollup_config: None,
                find_fork: false,
                timeout: 600,
            };
            let diagnostic = format!("{:?}", args.run().await.unwrap_err());
            assert!(diagnostic.contains(message), "{diagnostic}");
            assert!(!diagnostic.contains("secret-"), "{diagnostic}");
        }
        handle.stop().unwrap();
    }

    #[test]
    fn inspection_checks_custom_genesis_identity() {
        let directory = tempfile::tempdir().unwrap();
        let genesis_path = directory.path().join("genesis.json");
        let rollup_path = directory.path().join("rollup.json");
        let mut genesis = Genesis { timestamp: 10, ..Default::default() };
        genesis.config.chain_id = 123_456;
        std::fs::write(&genesis_path, serde_json::to_vec(&genesis).unwrap()).unwrap();
        let spec = BaseChainSpec::try_from_genesis(genesis).unwrap();
        let mut config = RollupConfig { l2_chain_id: 123_456.into(), ..Default::default() };
        config.genesis.l2.hash = spec.genesis_header.hash();
        config.genesis.l2.number = spec.genesis_header.number;
        config.genesis.l2_time = 10;
        std::fs::write(&rollup_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let args = InspectSnapshotArgs {
            rpc_url: "http://127.0.0.1:8545".parse().unwrap(),
            chain: genesis_path.to_string_lossy().into_owned(),
            rollup_config: Some(rollup_path.clone()),
            find_fork: false,
            timeout: 600,
        };
        args.resolved_chain().expect("matching custom genesis must be accepted");
        let mutations: [fn(&mut RollupConfig); 3] = [
            |config| config.genesis.l2_time += 1,
            |config| config.genesis.l2.number += 1,
            |config| config.genesis.l2.hash = Default::default(),
        ];
        for mutate in mutations {
            let mut changed = config.clone();
            mutate(&mut changed);
            std::fs::write(&rollup_path, serde_json::to_vec(&changed).unwrap()).unwrap();
            assert!(args.resolved_chain().is_err(), "mismatched L2 genesis must be rejected");
        }

        let mut genesis = Genesis { timestamp: 10, ..Default::default() };
        genesis.config.chain_id = 8453;
        std::fs::write(&genesis_path, serde_json::to_vec(&genesis).unwrap()).unwrap();
        let config = SnapshotChainConfig::default().resolve().unwrap().rollup_config;
        std::fs::write(&rollup_path, serde_json::to_vec(&*config).unwrap()).unwrap();
        assert!(args.resolved_chain().is_err(), "a built-in chain ID cannot mask another genesis");
        let args = InspectSnapshotArgs { rollup_config: None, ..args };
        assert!(
            args.resolved_chain().is_err(),
            "a genesis file must match without an override too"
        );
    }

    #[tokio::test]
    async fn inspection_rejects_malformed_urls_without_exposing_credentials() {
        let diagnostic = match DevnetCli::try_parse_from([
            "base-devnet",
            "inspect-snapshot",
            "--rpc-url",
            "http://secret-user:secret-password@host:bad/secret-path?token=secret-query",
        ]) {
            Ok(cli) => format!("{:?}", cli.run().await.unwrap_err()),
            Err(error) => error.to_string(),
        };
        assert!(diagnostic.contains("invalid --rpc-url: invalid port number"), "{diagnostic}");
        assert!(!diagnostic.contains("secret-"), "{diagnostic}");
    }

    #[tokio::test]
    async fn inspection_rollup_config_errors_name_the_file_step_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollup.json");
        for (contents, message) in [
            (None, "failed to read snapshot rollup config"),
            (Some(r#"{"genesis":"secret-contents"}"#), "failed to parse snapshot rollup config"),
        ] {
            if let Some(contents) = contents {
                std::fs::write(&path, contents).unwrap();
            }
            let args = InspectSnapshotArgs {
                rpc_url: "http://127.0.0.1:8545".into(),
                chain: "mainnet".into(),
                rollup_config: Some(path.clone()),
                find_fork: false,
                timeout: 600,
            };
            let diagnostic = format!("{:?}", args.run().await.unwrap_err());
            assert!(diagnostic.contains(message), "{diagnostic}");
            assert!(!diagnostic.contains("secret-"), "{diagnostic}");
        }
    }

    #[test]
    fn inspection_accepts_builtin_placeholder_genesis() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut args = InspectSnapshotArgs {
            rpc_url: "http://127.0.0.1:8545".into(),
            chain: "dev".into(),
            rollup_config: None,
            find_fork: false,
            timeout: 600,
        };
        let config = args.resolved_chain().unwrap().rollup_config;
        std::fs::write(file.path(), serde_json::to_vec(&*config).unwrap()).unwrap();
        args.rollup_config = Some(file.path().into());
        args.resolved_chain().unwrap();
    }

    #[test]
    fn rejects_inspect_snapshot_without_rpc_url() {
        assert!(DevnetCli::try_parse_from(["base-devnet", "inspect-snapshot"]).is_err());
    }
}
