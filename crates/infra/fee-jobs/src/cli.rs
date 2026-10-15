use std::{fmt, sync::Arc};

use alloy_primitives::Address;
use alloy_provider::{Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use base_cli_utils::{CliStyles, LogConfig};
use base_tx_manager::{
    NoopTxMetrics, RpcErrorClassifier, SignerConfig, SimpleTxManager, TxManagerConfig,
};
use clap::{Args, Parser, Subcommand};
use eyre::{WrapErr, ensure};
use tokio::time::timeout;
use url::Url;

use crate::{AlloyFeeReader, FeeJob, FeeJobKind};

base_cli_utils::define_log_args!("FEE_JOBS");
base_tx_manager::define_tx_manager_cli!("FEE_JOBS_TX_MANAGER", tx_send_timeout_default = "10m");

/// One-shot fee collection jobs.
#[derive(Debug, Parser)]
#[command(name = "fee-jobs", version, about, styles = CliStyles::init())]
pub struct Cli {
    /// Job to run.
    #[command(subcommand)]
    pub command: FeeCommand,
    /// Logging configuration.
    #[command(flatten)]
    pub logging: LogArgs,
}

/// Fee collection commands.
#[derive(Debug, Subcommand)]
pub enum FeeCommand {
    /// Disburse accumulated L2 fees when the disbursement interval has elapsed.
    Disburse(DisburseArgs),
    /// Process L1 fees when the tracker holds at least 1 ETH.
    Process(ProcessArgs),
    /// Sweep Basenames revenue when the controller holds at least 0.01 ETH.
    Sweep(SweepArgs),
}

/// Configuration shared by jobs without requiring other jobs' addresses or endpoints.
#[derive(Args)]
pub struct JobArgs {
    /// Local signing key in hex, with or without a 0x prefix.
    #[arg(long, env = "JOB_RUNNER_KEY", hide_env_values = true)]
    pub job_runner_key: String,
    /// Transaction management configuration.
    #[command(flatten)]
    pub tx_manager: TxManagerCli,
}

impl fmt::Debug for JobArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobArgs")
            .field("job_runner_key", &"[REDACTED]")
            .field("tx_manager", &self.tx_manager)
            .finish()
    }
}

impl JobArgs {
    /// Parses the signer without putting key material in an error.
    pub fn signer(&self) -> eyre::Result<PrivateKeySigner> {
        self.job_runner_key
            .parse()
            .map_err(|_| eyre::eyre!("JOB_RUNNER_KEY must be a valid 32-byte secp256k1 key in hex"))
    }
}

/// L2 disbursement configuration.
#[derive(Debug, Args)]
pub struct DisburseArgs {
    /// Base L2 HTTP RPC URL.
    #[arg(long, env = "L2_RPC_URL")]
    pub rpc_url: Url,
    /// L2 fee disburser address.
    #[arg(long, env = "L2_FEE_DISBURSER_ADDRESS")]
    pub contract_address: Address,
    /// Signing and transaction configuration.
    #[command(flatten)]
    pub job: JobArgs,
}

/// L1 processing configuration.
#[derive(Debug, Args)]
pub struct ProcessArgs {
    /// L1 HTTP RPC URL.
    #[arg(long, env = "L1_RPC_URL")]
    pub rpc_url: Url,
    /// L1 balance tracker address.
    #[arg(long, env = "L1_BALANCE_TRACKER_ADDRESS")]
    pub contract_address: Address,
    /// Signing and transaction configuration.
    #[command(flatten)]
    pub job: JobArgs,
}

/// L2 registrar sweep configuration.
#[derive(Debug, Args)]
pub struct SweepArgs {
    /// Base L2 HTTP RPC URL.
    #[arg(long, env = "L2_RPC_URL")]
    pub rpc_url: Url,
    /// Basenames registrar controller address.
    #[arg(long, env = "L2_REGISTRAR_CONTROLLER_ADDRESS")]
    pub contract_address: Address,
    /// Signing and transaction configuration.
    #[command(flatten)]
    pub job: JobArgs,
}

impl Cli {
    /// Initializes logging and runs the selected job, propagating failures to the entry point.
    pub async fn run(self) -> eyre::Result<()> {
        base_cli_utils::init_tracing!(LogConfig::from(self.logging))?;
        let (kind, rpc_url, contract, args) = match self.command {
            FeeCommand::Disburse(args) => {
                (FeeJobKind::Disburse, args.rpc_url, args.contract_address, args.job)
            }
            FeeCommand::Process(args) => {
                (FeeJobKind::Process, args.rpc_url, args.contract_address, args.job)
            }
            FeeCommand::Sweep(args) => {
                (FeeJobKind::Sweep, args.rpc_url, args.contract_address, args.job)
            }
        };
        ensure!(contract != Address::ZERO, "contract address must not be zero");
        ensure!(matches!(rpc_url.scheme(), "http" | "https"), "RPC URL must use HTTP or HTTPS");
        let signer = args.signer()?;
        let config = TxManagerConfig::try_from(args.tx_manager)?;
        let provider = ProviderBuilder::new().connect_http(rpc_url);
        let chain_id = timeout(config.network_timeout, provider.get_chain_id())
            .await
            .wrap_err("chain ID read timed out")?
            .map_err(|e| RpcErrorClassifier::classify_rpc_error(&e))?;
        let reader =
            AlloyFeeReader { provider: provider.clone(), network_timeout: config.network_timeout };
        let manager = SimpleTxManager::new(
            provider,
            SignerConfig::local(signer),
            config,
            chain_id,
            Arc::new(NoopTxMetrics),
        )
        .await
        .wrap_err("failed to initialize transaction manager")?
        .with_pending_nonce();
        let result = FeeJob { kind, contract }.run(&reader, &manager).await;
        manager.close();
        result?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use rstest::rstest;

    use super::{Cli, FeeCommand};

    #[rstest]
    #[case::disburse("disburse")]
    #[case::process("process")]
    #[case::sweep("sweep")]
    fn each_command_needs_only_its_own_configuration(#[case] command: &str) {
        let cli = Cli::try_parse_from([
            "fee-jobs",
            command,
            "--rpc-url",
            "http://localhost:8545",
            "--contract-address",
            "0x1111111111111111111111111111111111111111",
            "--job-runner-key",
            "invalid-secret",
        ])
        .unwrap();
        assert!(!format!("{cli:?}").contains("invalid-secret"));
        let args = match cli.command {
            FeeCommand::Disburse(args) => args.job,
            FeeCommand::Process(args) => args.job,
            FeeCommand::Sweep(args) => args.job,
        };
        let error = args.signer().unwrap_err();
        assert!(!format!("{error:?}").contains("invalid-secret"));
    }

    #[rstest]
    #[case::plain("")]
    #[case::prefixed("0x")]
    fn accepts_plain_and_prefixed_keys(#[case] prefix: &str) {
        let key = format!("{prefix}{}", "01".repeat(32));
        let cli = Cli::try_parse_from([
            "fee-jobs",
            "process",
            "--rpc-url",
            "http://localhost:8545",
            "--contract-address",
            "0x1111111111111111111111111111111111111111",
            "--job-runner-key",
            &key,
        ])
        .unwrap();
        let FeeCommand::Process(args) = cli.command else { panic!("expected process command") };
        assert!(args.job.signer().is_ok());
        assert!(!format!("{args:?}").contains(&key));
    }
}
