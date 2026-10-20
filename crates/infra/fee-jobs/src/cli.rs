use std::{env, ffi::OsString, fmt, sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_provider::{Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use base_cli_utils::{CliStyles, LogConfig};
use base_tx_manager::{
    NoopTxMetrics, RpcErrorClassifier, SignerConfig, SimpleTxManager, TxManagerConfig,
};
use base_withdrawer::{RpcUrlValueParser, Secret, SecretValueParser, WithdrawerArgs};
use clap::{Args, Command, CommandFactory, FromArgMatches, Parser, Subcommand};
use eyre::{WrapErr, ensure};
use tokio::time::timeout;
use url::Url;

use crate::{AlloyFeeReader, ConfirmJob, FeeJob, FeeJobKind};

base_cli_utils::define_log_args!("FEE_JOBS");
base_tx_manager::define_tx_manager_cli!("FEE_JOBS_TX_MANAGER", tx_send_timeout_default = "10m");

/// One-shot fee collection jobs with sensitive values redacted from parser errors.
#[derive(Debug, Args)]
pub struct Cli {
    /// Job to run.
    #[command(subcommand)]
    pub command: FeeCommand,
    /// Logging configuration.
    #[command(flatten)]
    pub logging: LogArgs,
}

impl CommandFactory for Cli {
    fn command() -> Command {
        Self::augment_args(
            Command::new("fee-jobs").version(env!("CARGO_PKG_VERSION")).styles(CliStyles::init()),
        )
        .about("One-shot fee collection jobs.")
    }

    fn command_for_update() -> Command {
        Self::augment_args_for_update(
            Command::new("fee-jobs").version(env!("CARGO_PKG_VERSION")).styles(CliStyles::init()),
        )
        .about("One-shot fee collection jobs.")
    }
}

impl Parser for Cli {
    fn parse() -> Self {
        Self::parse_from(env::args_os())
    }

    fn try_parse() -> Result<Self, clap::Error> {
        Self::try_parse_from(env::args_os())
    }

    fn parse_from<I, T>(itr: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        Self::try_parse_from(itr).unwrap_or_else(|error| error.exit())
    }

    fn try_parse_from<I, T>(itr: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let mut matches = WithdrawerArgs::parse_matches(Self::command(), itr)?;
        Self::from_arg_matches_mut(&mut matches)
    }

    fn update_from<I, T>(&mut self, itr: I)
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        self.try_update_from(itr).unwrap_or_else(|error| error.exit());
    }

    fn try_update_from<I, T>(&mut self, itr: I) -> Result<(), clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let mut matches = WithdrawerArgs::parse_matches(Self::command_for_update(), itr)?;
        self.update_from_arg_matches_mut(&mut matches)
    }
}

/// Fee collection commands.
#[derive(Debug, Subcommand)]
pub enum FeeCommand {
    /// Prove and finalize recent fee disbursals on L1.
    Confirm(ConfirmArgs),
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

/// Fault-proof fee disbursal confirmation configuration.
#[derive(Args)]
pub struct ConfirmArgs {
    /// L1 HTTP RPC URL.
    #[arg(long, env = "L1_RPC_URL", hide_env_values = true, value_parser = RpcUrlValueParser)]
    pub l1_rpc_url: Url,
    /// Base L2 HTTP RPC URL.
    #[arg(long, env = "L2_RPC_URL", hide_env_values = true, value_parser = RpcUrlValueParser)]
    pub l2_rpc_url: Url,
    /// L1 Optimism portal proxy.
    #[arg(long, env = "L1_OPTIMISM_PORTAL_ADDRESS")]
    pub l1_optimism_portal_address: Address,
    /// L2 fee disburser to query.
    #[arg(long, env = "L2_FEE_DISBURSER_ADDRESS")]
    pub l2_fee_disburser_address: Address,
    /// Local signing key in hex, with or without a 0x prefix.
    #[arg(long, env = "JOB_RUNNER_KEY", hide_env_values = true, value_parser = SecretValueParser::<String>::default())]
    pub job_runner_key: Secret<String>,
    /// L2 lookback in blocks; must be smaller than the latest block number.
    #[arg(long, env = "FEE_JOBS_CONFIRM_NUM_BLOCKS_TO_QUERY", default_value_t = 1_296_000)]
    pub num_blocks_to_query: u64,
    /// Maximum inclusive block count per log query.
    #[arg(long, env = "FEE_JOBS_CONFIRM_NUM_BLOCKS_PER_QUERY", default_value_t = 3000, value_parser = clap::value_parser!(u64).range(1..))]
    pub num_blocks_per_query: u64,
    /// Prepare transactions without signing or broadcasting them.
    #[arg(long)]
    pub dry_run: bool,
    /// Deadline per withdrawal, including decisions, proof verification and submission.
    #[arg(long, env = "FEE_JOBS_CONFIRM_WITHDRAWAL_TIMEOUT", default_value = "10m", value_parser = humantime::parse_duration)]
    pub withdrawal_timeout: Duration,
}

impl fmt::Debug for ConfirmArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfirmArgs")
            .field("l1_rpc_url", &"[REDACTED]")
            .field("l2_rpc_url", &"[REDACTED]")
            .field("l1_optimism_portal_address", &self.l1_optimism_portal_address)
            .field("l2_fee_disburser_address", &self.l2_fee_disburser_address)
            .field("job_runner_key", &self.job_runner_key)
            .field("num_blocks_to_query", &self.num_blocks_to_query)
            .field("num_blocks_per_query", &self.num_blocks_per_query)
            .field("dry_run", &self.dry_run)
            .field("withdrawal_timeout", &self.withdrawal_timeout)
            .finish()
    }
}

impl Cli {
    /// Initializes logging and runs the selected job, propagating failures to the entry point.
    pub async fn run(self) -> eyre::Result<()> {
        base_cli_utils::init_tracing!(LogConfig::from(self.logging))?;
        let (kind, rpc_url, contract, args) = match self.command {
            FeeCommand::Confirm(args) => return ConfirmJob { args }.run().await,
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
    use std::{env, process::Command, time::Duration};

    use alloy_primitives::Address;
    use clap::{Parser, error::ErrorKind};
    use rstest::rstest;

    use super::{Cli, FeeCommand};
    use crate::{ConfirmArgs, ConfirmJob};

    fn confirm(extra: &[&str]) -> Result<ConfirmArgs, clap::Error> {
        let mut argv = vec![
            "fee-jobs",
            "confirm",
            "--l1-rpc-url",
            "https://l1.example",
            "--l2-rpc-url",
            "https://l2.example",
            "--l1-optimism-portal-address",
            "0x1111111111111111111111111111111111111111",
            "--l2-fee-disburser-address",
            "0x4444444444444444444444444444444444444444",
            "--job-runner-key",
            "invalid-secret",
        ];
        argv.extend_from_slice(extra);
        let FeeCommand::Confirm(args) = Cli::try_parse_from(argv)?.command else { unreachable!() };
        Ok(args)
    }

    #[test]
    fn confirm_reads_deployment_environment_and_defaults() {
        if env::var_os("FEE_JOBS_CONFIRM_ENV_TEST_CHILD").is_some() {
            let FeeCommand::Confirm(args) =
                Cli::try_parse_from(["fee-jobs", "confirm"]).unwrap().command
            else {
                panic!("expected confirm command");
            };
            assert_eq!(args.l1_rpc_url.as_str(), "https://l1.example/");
            assert_eq!(args.l2_rpc_url.as_str(), "https://l2.example/");
            assert_eq!(args.l1_optimism_portal_address, Address::repeat_byte(0x11));
            assert_eq!(args.l2_fee_disburser_address, Address::repeat_byte(0x44));
            assert!(args.job_runner_key.0 == "01".repeat(32));
            assert_eq!(args.num_blocks_to_query, 100);
            assert_eq!(args.num_blocks_per_query, 25);
            assert_eq!(args.withdrawal_timeout, Duration::from_secs(120));
            return;
        }
        let args = confirm(&[]).unwrap();
        assert_eq!(args.num_blocks_to_query, 1_296_000);
        assert_eq!(args.num_blocks_per_query, 3000);
        assert_eq!(args.withdrawal_timeout, Duration::from_secs(600));
        let output = Command::new(env::current_exe().unwrap())
            .args(["--exact", "cli::tests::confirm_reads_deployment_environment_and_defaults"])
            .env_clear()
            .env("FEE_JOBS_CONFIRM_ENV_TEST_CHILD", "1")
            .env("L1_RPC_URL", "https://l1.example")
            .env("L2_RPC_URL", "https://l2.example")
            .env("L1_OPTIMISM_PORTAL_ADDRESS", Address::repeat_byte(0x11).to_string())
            .env("L2_FEE_DISBURSER_ADDRESS", Address::repeat_byte(0x44).to_string())
            .env("JOB_RUNNER_KEY", "01".repeat(32))
            .env("FEE_JOBS_CONFIRM_NUM_BLOCKS_TO_QUERY", "100")
            .env("FEE_JOBS_CONFIRM_NUM_BLOCKS_PER_QUERY", "25")
            .env("FEE_JOBS_CONFIRM_WITHDRAWAL_TIMEOUT", "2m")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
    }

    #[test]
    fn parser_redacts_unexpected_arguments_and_invalid_values_for_every_command() {
        let key = "01".repeat(32);
        let key_flag = format!("--job-runner-key={key}");
        let url = "https://rpc-user:rpc-password@rpc.example/path-secret?key=query-secret";
        for command in ["confirm", "disburse", "process", "sweep"] {
            let invalid_flags: &[&str] = if command == "confirm" {
                &[
                    "--num-blocks-per-query",
                    "--withdrawal-timeout",
                    "--l1-optimism-portal-address",
                    "--l1-rpc-url",
                ]
            } else {
                &["--contract-address", "--rpc-url"]
            };
            for value in [key.as_str(), url] {
                for flag in std::iter::once(None).chain(invalid_flags.iter().copied().map(Some)) {
                    let mut argv = vec!["fee-jobs", command, &key_flag];
                    if let Some(flag) = flag {
                        argv.push(flag);
                    }
                    let rejected = if flag.is_some_and(|flag| flag.ends_with("rpc-url")) {
                        format!("invalid URL {value}")
                    } else {
                        value.to_owned()
                    };
                    argv.push(&rejected);
                    let error = Cli::try_parse_from(&argv).unwrap_err();
                    if flag.is_none() {
                        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
                    } else {
                        assert!(matches!(
                            error.kind(),
                            ErrorKind::ValueValidation | ErrorKind::InvalidValue
                        ));
                    }
                    let mut cli = Cli {
                        command: FeeCommand::Confirm(confirm(&[]).unwrap()),
                        logging: Default::default(),
                    };
                    let update_error = cli.try_update_from(argv).unwrap_err();
                    let diagnostic = format!("{error} {error:?} {update_error} {update_error:?}");
                    for secret in
                        [key.as_str(), "rpc-user", "rpc-password", "path-secret", "query-secret"]
                    {
                        assert!(!diagnostic.contains(secret), "secret leaked: {diagnostic}");
                    }
                }
            }
        }
    }

    #[test]
    fn confirm_help_and_debug_hide_rpc_credentials() {
        const URL: &str = "https://rpc-user:rpc-password@rpc.example/path-secret?key=query-secret";
        if env::var_os("FEE_JOBS_TEST_CHILD").is_some() {
            let help = Cli::try_parse_from(["fee-jobs", "confirm", "--help"]).unwrap_err();
            assert_eq!(help.kind(), ErrorKind::DisplayHelp);
            let mut args = confirm(&[]).unwrap();
            args.l1_rpc_url = env::var("L1_RPC_URL").unwrap().parse().unwrap();
            args.l2_rpc_url = env::var("L2_RPC_URL").unwrap().parse().unwrap();
            let job = ConfirmJob { args };
            let diagnostic = format!("{help} {:?} {job:?}", job.args);
            assert!(diagnostic.contains("L1_RPC_URL") && diagnostic.contains("L2_RPC_URL"));
            for secret in ["rpc-user", "rpc-password", "rpc.example", "path-secret", "query-secret"]
            {
                assert!(!diagnostic.contains(secret), "credentials leaked: {diagnostic}");
            }
            return;
        }
        let output = Command::new(env::current_exe().unwrap())
            .args(["--exact", "cli::tests::confirm_help_and_debug_hide_rpc_credentials"])
            .env("FEE_JOBS_TEST_CHILD", "1")
            .env("L1_RPC_URL", URL)
            .env("L2_RPC_URL", URL)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
    }

    #[test]
    fn parser_preserves_help_version_and_missing_value_errors() {
        let help = Cli::try_parse_from(["fee-jobs", "--help"]).unwrap_err();
        assert_eq!(help.kind(), ErrorKind::DisplayHelp);
        assert!(help.to_string().contains("One-shot fee collection jobs."));
        let version = Cli::try_parse_from(["fee-jobs", "--version"]).unwrap_err();
        assert_eq!(version.kind(), ErrorKind::DisplayVersion);
        assert!(version.to_string().contains(env!("CARGO_PKG_VERSION")));
        let missing = Cli::try_parse_from(["fee-jobs", "confirm", "--l1-rpc-url"]).unwrap_err();
        assert!(
            missing.to_string().contains("a value is required for '--l1-rpc-url <L1_RPC_URL>'")
        );
    }

    #[test]
    fn confirm_rejects_invalid_keys_without_disclosing_them() {
        let job = ConfirmJob { args: confirm(&[]).unwrap() };
        assert!(!format!("{job:?}").contains("invalid-secret"));
        let error = job.config().unwrap_err();
        assert!(!format!("{error:?}").contains("invalid-secret"));
        assert!(format!("{error:#}").contains("JOB_RUNNER_KEY"));
    }

    #[test]
    fn confirm_rejects_zero_addresses_non_http_urls_and_zero_limits() {
        for address in ["--l1-optimism-portal-address", "--l2-fee-disburser-address"] {
            let mut args = confirm(&[]).unwrap();
            let zero = Default::default();
            match address {
                "--l1-optimism-portal-address" => args.l1_optimism_portal_address = zero,
                _ => args.l2_fee_disburser_address = zero,
            }
            assert!(ConfirmJob { args }.config().unwrap_err().to_string().contains("zero"));
        }
        for l1 in [true, false] {
            let mut args = confirm(&[]).unwrap();
            if l1 {
                args.l1_rpc_url = "ws://user:password@secret.endpoint".parse().unwrap();
            } else {
                args.l2_rpc_url = "file:///secret".parse().unwrap();
            }
            let error = ConfirmJob { args }.config().unwrap_err();
            assert!(format!("{error:#}").contains("HTTP"));
            assert!(!format!("{error:?}").contains("secret"));
        }
        assert!(confirm(&["--num-blocks-per-query", "0"]).is_err());
        let args = confirm(&["--withdrawal-timeout", "0s"]).unwrap();
        assert!(ConfirmJob { args }.config().unwrap_err().to_string().contains("positive"));
    }

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
            FeeCommand::Confirm(_) => panic!("expected a transaction-manager job"),
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
