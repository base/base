//! Command-line launcher for development networks.

use std::{
    io::Write as _,
    num::NonZeroU64,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use alloy_primitives::{Address, B256};
use clap::{Args, Parser, Subcommand};
use eyre::{Result, WrapErr};
use serde::Serialize;

use crate::{
    DevnetBlockInterval, DevnetConfig, DevnetL2State, DevnetPrefund, DevnetSnapshotHead, SharedL1,
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
    /// Start a CI-scoped shared L1 and write its runtime manifest.
    SharedL1(SharedL1Args),
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
    /// Maximum time to wait for graceful shutdown after interruption. Zero terminates the process
    /// immediately because snapshot datadirs are caller-owned and expected to be disposable.
    #[arg(long, default_value_t = 0)]
    pub shutdown_timeout_seconds: u64,
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
        println!("send Ctrl-C or SIGTERM to stop");

        if self.shutdown_timeout_seconds == 0 {
            return Self::wait_for_fast_shutdown().await;
        }

        Self::wait_for_shutdown_signal().await?;
        Self::shutdown_with_deadline(stack, self.shutdown_timeout_seconds).await
    }

    /// Replaces Reth's graceful process handlers with async-signal-safe immediate exit handlers.
    ///
    /// The handler itself must not allocate or run destructors. `_exit` guarantees that a
    /// saturated Tokio runtime or non-cancellable state-root task cannot delay benchmark cleanup.
    #[cfg(unix)]
    async fn wait_for_fast_shutdown() -> Result<()> {
        unsafe extern "C" fn exit_immediately(_: libc::c_int) {
            // SAFETY: `_exit` is async-signal-safe and terminates without running destructors.
            unsafe { libc::_exit(0) }
        }

        for signal in [libc::SIGINT, libc::SIGTERM] {
            // SAFETY: `exit_immediately` has the required C signal-handler ABI and only calls the
            // async-signal-safe `_exit` function.
            let previous = unsafe {
                libc::signal(signal, exit_immediately as *const () as libc::sighandler_t)
            };
            if previous == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error())
                    .wrap_err("failed to install immediate snapshot shutdown handler");
            }
        }

        std::future::pending().await
    }

    /// Waits for Ctrl-C before immediately terminating on platforms without Unix signals.
    #[cfg(not(unix))]
    async fn wait_for_fast_shutdown() -> Result<()> {
        tokio::signal::ctrl_c().await.wrap_err("failed to listen for Ctrl-C")?;
        std::process::exit(0)
    }

    /// Gracefully shuts down the stack, but terminates the process if runtime destruction remains
    /// blocked after the deadline. A native thread is used because the Tokio runtime itself may be
    /// the component waiting on a non-cancellable blocking state-root task.
    async fn shutdown_with_deadline(stack: SnapshotL2Stack, timeout_seconds: u64) -> Result<()> {
        let complete = Arc::new(AtomicBool::new(false));
        let watchdog_complete = Arc::clone(&complete);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(timeout_seconds));
            if !watchdog_complete.load(Ordering::Acquire) {
                eprintln!(
                    "snapshot devnet shutdown exceeded {timeout_seconds}s; forcing failed process exit"
                );
                let _ = std::io::stderr().flush();
                std::process::exit(1);
            }
        });

        let result = stack.shutdown().await;
        complete.store(true, Ordering::Release);
        result
    }

    /// Waits for the interactive or process-manager shutdown signals used by the launcher.
    #[cfg(unix)]
    async fn wait_for_shutdown_signal() -> Result<()> {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigterm =
            signal(SignalKind::terminate()).wrap_err("failed to install SIGTERM handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.wrap_err("failed to listen for Ctrl-C")?;
            }
            _ = sigterm.recv() => {}
        }
        Ok(())
    }

    /// Waits for the interactive shutdown signal used by the launcher.
    #[cfg(not(unix))]
    async fn wait_for_shutdown_signal() -> Result<()> {
        tokio::signal::ctrl_c().await.wrap_err("failed to listen for Ctrl-C")
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
    use std::num::NonZeroU64;

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

        let DevnetCommand::Snapshot(args) = cli.command else {
            panic!("expected snapshot command")
        };
        assert_eq!(args.chain, "sepolia");
        assert_eq!(args.builder_datadir.to_str(), Some("/tmp/builder"));
        assert!(args.prefund_address.is_some());
        assert_eq!(args.block_interval, DevnetBlockInterval::TwoHundredMilliseconds);
        assert!(args.block_gas_limit.is_none());
        assert_eq!(args.shutdown_timeout_seconds, 0);
    }

    #[test]
    fn parses_snapshot_shutdown_timeout() {
        let cli = DevnetCli::try_parse_from([
            "base-devnet",
            "snapshot",
            "--builder-datadir",
            "/tmp/builder",
            "--client-datadir",
            "/tmp/client",
            "--shutdown-timeout-seconds",
            "30",
            "--block-gas-limit",
            "12000000000",
        ])
        .unwrap();

        let DevnetCommand::Snapshot(args) = cli.command else {
            panic!("expected snapshot command")
        };
        assert_eq!(args.shutdown_timeout_seconds, 30);
        assert_eq!(args.block_gas_limit.map(NonZeroU64::get), Some(12_000_000_000));
    }
}
