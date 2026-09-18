//! Top-level command dispatch for the unified Base binary.

use base_batcher_cli::BatcherArgs;
use base_cli_utils::RuntimeManager;
use base_common_chains::BaseUpgrade;
use base_execution_cli::{chainspec::BaseChainSpecParser, commands::base_proofs};
use base_node_core::BaseNode;
use clap::{Args, Subcommand};
use reth_cli_runner::CliRunner;

use crate::{
    commands::{
        bootnode::BootnodeCommand, follow::FollowCommand, reth::RethCommand, rpc::RpcCommand,
        sequencer::SequencerCommand, snapshot::SnapshotCommand, update::UpdateCommand,
    },
    config::ChainResolver,
};

/// CLI inputs for the offline genesis workflow.
#[cfg(feature = "genesis")]
#[derive(Debug, Args)]
#[group(id = "GenesisUpgradeArgs")]
pub(crate) struct GenesisCommand {
    /// Genesis workflow inputs and generated network configuration.
    #[command(flatten)]
    pub command: base_genesis::GenesisCommand,
    /// Optional Isthmus activation block.
    #[arg(long = "isthmus-block", env = "L2_ISTHMUS_BLOCK")]
    pub isthmus: Option<u64>,
    /// Optional Azul activation block.
    #[arg(long = "azul-block", env = "L2_BASE_AZUL_BLOCK")]
    pub azul: Option<u64>,
    /// Optional Beryl activation block.
    #[arg(long = "beryl-block", env = "L2_BASE_BERYL_BLOCK")]
    pub beryl: Option<u64>,
    /// Optional Cobalt activation block.
    #[arg(long = "cobalt-block", env = "L2_BASE_COBALT_BLOCK")]
    pub cobalt: Option<u64>,
    /// Optional Denim activation block.
    #[arg(long = "denim-block", env = "L2_BASE_DENIM_BLOCK")]
    pub denim: Option<u64>,
    /// Optional Zenith activation block.
    #[arg(long = "zenith-block", env = "L2_BASE_ZENITH_BLOCK")]
    pub zenith: Option<u64>,
}

#[cfg(feature = "genesis")]
impl GenesisCommand {
    /// Run the genesis workflow with command-line upgrade overrides.
    pub(crate) fn run(self) -> eyre::Result<()> {
        base_genesis::GenesisBuilder::generate_with_upgrade_blocks(
            self.command,
            &[
                (BaseUpgrade::Isthmus, self.isthmus),
                (BaseUpgrade::Azul, self.azul),
                (BaseUpgrade::Beryl, self.beryl),
                (BaseUpgrade::Cobalt, self.cobalt),
                (BaseUpgrade::Denim, self.denim),
                (BaseUpgrade::Zenith, self.zenith),
            ]
            .into_iter()
            .filter_map(|(upgrade, block)| block.map(|block| (upgrade, block)))
            .collect::<Vec<_>>(),
        )
    }
}

/// Top-level commands for `base`.
#[derive(Subcommand, Debug)]
#[non_exhaustive]
pub(crate) enum BaseCommand {
    /// Assemble Base genesis inputs and state (full workflow: `just genesis`).
    #[cfg(feature = "genesis")]
    Genesis(Box<GenesisCommand>),
    /// Submit L2 batch data to L1.
    #[command(name = "batcher", hide = true)]
    Batcher(Box<BatcherArgs>),
    /// Run consensus and execution discovery-only bootnodes.
    #[command(name = "bootnode")]
    Bootnode(Box<BootnodeCommand>),
    /// Run the integrated node in RPC mode.
    #[command(name = "rpc")]
    Rpc(Box<RpcCommand>),
    /// Run the integrated node in follow mode (execution + consensus follow node).
    #[command(name = "follow")]
    Follow(Box<FollowCommand>),
    /// Run integrated execution, builder, and consensus services in sequencer mode.
    #[command(name = "sequencer")]
    Sequencer(Box<SequencerCommand>),
    /// Update the base binary to the latest release.
    #[command(name = "update")]
    Update(Box<UpdateCommand>),
    /// Execution-layer maintenance utilities (use this group's own --chain flag).
    #[command(name = "reth")]
    Reth(Box<RethCommand>),
    /// Manage storage of historical proofs in the fault-proof window (uses its own --chain flag).
    #[command(name = "proofs")]
    Proofs(Box<base_proofs::Command<BaseChainSpecParser>>),
    /// Snapshot manifest generation and download utilities (uses its own --chain flag).
    #[command(name = "snapshot")]
    Snapshot(Box<SnapshotCommand>),
}

impl BaseCommand {
    pub(crate) fn run(
        self,
        chain_resolver: ChainResolver,
        metrics_enabled: bool,
    ) -> eyre::Result<()> {
        match self {
            #[cfg(feature = "genesis")]
            Self::Genesis(command) => {
                chain_resolver.reject_for_reth_command("base genesis")?;
                (*command).run()
            }
            Self::Batcher(batcher) => {
                chain_resolver.reject_for_reth_command("base batcher")?;
                RuntimeManager::new().run_until_ctrl_c((*batcher).exec(metrics_enabled))
            }
            Self::Bootnode(bootnode) => (*bootnode).run(chain_resolver.resolve()?, metrics_enabled),
            Self::Rpc(rpc) => (*rpc).run(chain_resolver.resolve()?),
            Self::Follow(follow) => (*follow).run(chain_resolver.resolve()?),
            Self::Sequencer(sequencer) => (*sequencer).run(chain_resolver.resolve()?),
            Self::Update(update) => (*update).run(),
            Self::Reth(reth) => {
                chain_resolver.reject_for_reth_command("base reth")?;
                (*reth).run()
            }
            Self::Proofs(command) => {
                chain_resolver.reject_for_reth_command("base proofs")?;
                let runner = CliRunner::try_default_runtime()?;
                let runtime = runner.runtime();
                runner.run_blocking_until_ctrl_c((*command).execute::<BaseNode>(runtime))
            }
            Self::Snapshot(snapshot) => {
                chain_resolver.reject_for_reth_command("base snapshot")?;
                (*snapshot).run()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, process::Command, thread, time::Duration};

    use clap::Parser;

    use super::BaseCommand;
    use crate::{cli::BaseCli, config::ChainResolver};

    #[cfg(feature = "genesis")]
    #[test]
    fn genesis_accepts_validator_count() {
        let cli = BaseCli::try_parse_from(["base", "genesis", "--validator-count", "64"]).unwrap();
        let BaseCommand::Genesis(command) = cli.command else {
            panic!("expected genesis command");
        };
        assert_eq!(command.command.config.validator_count.get(), 64);
    }

    #[cfg(feature = "genesis")]
    #[test]
    fn accepts_genesis_upgrade_blocks() {
        let cli = BaseCli::try_parse_from([
            "base",
            "genesis",
            "--isthmus-block",
            "20",
            "--denim-block",
            "100",
            "--zenith-block",
            "105",
        ])
        .unwrap();
        let BaseCommand::Genesis(command) = cli.command else { panic!("expected genesis command") };

        assert_eq!(command.isthmus, Some(20));
        assert_eq!(command.denim, Some(100));
        assert_eq!(command.zenith, Some(105));
    }

    #[test]
    fn rejects_legacy_node_rpc_path() {
        let err = BaseCli::try_parse_from(["base", "node", "rpc"]).unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("node"));
    }

    #[test]
    fn rejects_legacy_flat_db_path() {
        let err = BaseCli::try_parse_from(["base", "db", "--help"]).unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("db"));
    }

    #[test]
    fn rejects_legacy_flat_snapshot_manifest_path() {
        let err = BaseCli::try_parse_from(["base", "snapshot-manifest", "--help"]).unwrap_err();

        let rendered = err.to_string();
        assert!(rendered.contains("snapshot-manifest"));
    }

    #[test]
    fn accepts_reth_help() {
        let err = BaseCli::try_parse_from(["base", "reth", "--help"]).unwrap_err();

        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn accepts_snapshot_help() {
        let err = BaseCli::try_parse_from(["base", "snapshot", "--help"]).unwrap_err();

        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn rejects_top_level_chain_for_reth_subcommands() {
        let cli =
            BaseCli::try_parse_from(["base", "--chain", "sepolia", "reth", "db", "stats"]).unwrap();
        let err = cli.command.run(ChainResolver::new(cli.chain), false).unwrap_err();

        assert!(err.to_string().contains("base reth"));
        assert!(err.to_string().contains("base --chain"));
    }

    #[test]
    fn unified_upgrade_metrics_without_standalone_endpoint() {
        // Each launch needs its own process: reth owns global recorder/thread-pool state.
        // Keep temporary files alive until the child (including its node thread) has exited.
        for flavor in ["rpc", "sequencer", "follow"] {
            let dir = tempfile::tempdir().unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            for (key, _) in std::env::vars() {
                if key.starts_with("BASE_") || key.starts_with("OP_RETH_") {
                    command.env_remove(key);
                }
            }
            let output = command
                .current_dir(dir.path())
                .args([
                    "--exact",
                    "commands::command::tests::unified_upgrade_metrics_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("BASE_NODE_METRICS_ENABLED", "false")
                .env("BASE_METRICS_TEST_FLAVOR", flavor)
                .env("BASE_METRICS_TEST_DIR", dir.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{flavor} metrics test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }

    #[tokio::test]
    #[ignore = "spawned by unified_upgrade_metrics_without_standalone_endpoint"]
    async fn unified_upgrade_metrics_child() {
        let flavor = std::env::var("BASE_METRICS_TEST_FLAVOR").unwrap();
        let dir = std::env::var("BASE_METRICS_TEST_DIR").unwrap();
        let metrics_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let metrics_addr = metrics_listener.local_addr().unwrap().to_string();
        // Leave upstream requests pending locally: this test needs execution startup and a
        // metrics scrape, not a live L1, beacon node, or follow source.
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_url = format!("http://{}", upstream.local_addr().unwrap());
        let ipc = format!("{dir}/rpc.ipc");
        let mut args = vec![
            "base",
            "--chain",
            "dev",
            &flavor,
            "--datadir",
            &dir,
            "--ipcpath",
            &ipc,
            "--metrics",
            &metrics_addr,
            "--port",
            "0",
            "--authrpc.port",
            "0",
            "--rpc.port",
            "0",
            "--disable-discovery",
            "--l1-eth-rpc",
            &upstream_url,
            "--l1-beacon",
            &upstream_url,
        ];
        if flavor == "follow" {
            args.extend(["--source-l2-rpc", &upstream_url, "--l2-rpc-url", &upstream_url]);
        } else {
            args.extend(["--p2p.listen.tcp", "0", "--p2p.listen.udp", "0"]);
        }
        if flavor == "sequencer" {
            args.extend([
                "--p2p.sequencer.key",
                "bcc617ea05150ff60490d3c6058630ba94ae9f12a02a87efd291349ca0e54e0a",
                "--flashblocks.port",
                "0",
            ]);
        }
        let cli = BaseCli::parse_from(args);
        assert!(!cli.metrics.enabled);
        drop(metrics_listener);
        // Debug builds of the unified launch future need more than the default worker stack.
        let node =
            thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(move || cli.run()).unwrap();
        let client = reqwest::Client::builder().timeout(Duration::from_secs(1)).build().unwrap();
        let metrics_url = format!("http://{metrics_addr}/metrics");
        let scrape = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if node.is_finished() {
                    panic!(
                        "node exited before emitting upgrade metrics: {:?}",
                        node.join().unwrap()
                    );
                }
                if let Ok(response) = client.get(&metrics_url).send().await
                    && let Ok(body) = response.text().await
                    && [("Azul", 0), ("Beryl", -1), ("Cobalt", -1)].iter().all(|(upgrade, time)| {
                        body.lines().any(|line| {
                            line == format!(
                                "reth_base_node_upgrades{{upgrade=\"{upgrade}\"}} {time}"
                            )
                        })
                    }) && body.lines().any(|line| {
                    line == "reth_base_upgrade_signal_mode_info{layer=\"el\",mode=\"disabled\"} 1"
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        assert!(scrape.is_ok(), "missing startup upgrade gauges on the reth metrics endpoint");
    }
}
