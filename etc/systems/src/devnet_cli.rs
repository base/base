//! Command-line launcher for development networks.

use std::path::PathBuf;

use alloy_primitives::{Address, B256};
use clap::{Args, Parser, Subcommand};
use eyre::{Result, WrapErr};
use serde::Serialize;

use crate::{
    DevnetBlockInterval, DevnetConfig, DevnetL2State, DevnetPrefund, DevnetSnapshotHead,
    SnapshotChainConfig, SnapshotL2Stack, SystemTestStackBuilder,
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
        }
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
            builder_rpc_url: stack.builder_rpc_url()?.to_string(),
            builder_flashblocks_url: stack.builder_flashblocks_url()?.to_string(),
            client_rpc_url: stack.client_rpc_url()?.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{DevnetCli, DevnetCommand};
    use crate::DevnetBlockInterval;

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

        let DevnetCommand::Snapshot(args) = cli.command;
        assert_eq!(args.chain, "sepolia");
        assert_eq!(args.builder_datadir.to_str(), Some("/tmp/builder"));
        assert!(args.prefund_address.is_some());
        assert_eq!(args.block_interval, DevnetBlockInterval::TwoHundredMilliseconds);
    }
}
