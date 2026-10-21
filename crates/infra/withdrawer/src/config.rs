use std::{
    env,
    ffi::{OsStr, OsString},
    time::Duration,
};

use alloy_primitives::{Address, B256};
use clap::{
    Arg, ArgGroup, ArgMatches, Args, Command, CommandFactory, FromArgMatches, Parser,
    builder::TypedValueParser,
    error::{ContextKind, ContextValue, ErrorKind},
};
use coins_bip32::path::DerivationPath;
use coins_bip39::{English, Mnemonic};
use thiserror::Error;
use url::Url;

use crate::{FaultProofContracts, Network, NetworkPreset, ProofSystem, Secret, SecretValueParser};

base_tx_manager::define_tx_manager_cli!("WITHDRAWER_TX_MANAGER", tx_send_timeout_default = "10m");

// Set after `augment_args`, which otherwise uses the rustdoc of `WithdrawerArgs` as the help text.
const WITHDRAWER_ABOUT: &str = "Prove or finalize OP Stack withdrawals";

/// Parses RPC endpoints without including rejected values in clap errors.
#[derive(Debug, Clone)]
pub struct RpcUrlValueParser;

impl TypedValueParser for RpcUrlValueParser {
    type Value = Url;

    fn parse_ref(
        &self,
        _cmd: &Command,
        arg: Option<&Arg>,
        value: &OsStr,
    ) -> Result<Self::Value, clap::Error> {
        value
            .to_str()
            .map_or_else(
                || Err("check its format".to_owned()),
                |value| Url::parse(value).map_err(|error| error.to_string()),
            )
            .map_err(|reason| {
                let message = arg.and_then(Arg::get_long).map_or_else(
                    || format!("invalid RPC URL: {reason}"),
                    |flag| format!("invalid RPC URL for --{flag}: {reason}"),
                );
                clap::Error::raw(ErrorKind::ValueValidation, message)
            })
    }
}

/// Parses derivation paths with fixed error reasons and no rejected values in clap errors.
#[derive(Debug, Clone)]
pub struct HdPathValueParser;

impl TypedValueParser for HdPathValueParser {
    type Value = DerivationPath;

    fn parse_ref(
        &self,
        _cmd: &Command,
        arg: Option<&Arg>,
        value: &OsStr,
    ) -> Result<Self::Value, clap::Error> {
        value
            .to_str()
            .ok_or("invalid derivation path")
            .and_then(WithdrawerArgs::parse_hd_path)
            .map_err(|reason| {
                let message = arg.and_then(Arg::get_long).map_or_else(
                    || format!("invalid derivation path: {reason}"),
                    |flag| format!("invalid derivation path for --{flag}: {reason}"),
                );
                clap::Error::raw(ErrorKind::ValueValidation, message)
            })
    }
}

/// Command-line withdrawal settings before cross-field validation.
#[derive(Debug, Clone, Args)]
#[command(name = "withdrawer")]
#[command(group(ArgGroup::new("signer").required(true).multiple(false)))]
#[command(group(ArgGroup::new("proofs").args(["dispute_game_factory", "l2_output_oracle"])))]
pub struct WithdrawerArgs {
    /// L1 RPC endpoint.
    #[arg(long, value_parser = RpcUrlValueParser)]
    pub l1_rpc_url: Url,
    /// L2 withdrawal transaction hash.
    #[arg(long)]
    pub withdrawal: B256,
    /// Built-in network, defaulting to base-mainnet without custom contracts.
    #[arg(long, value_enum, conflicts_with_all = ["l2_rpc_url", "portal", "dispute_game_factory", "anchor_state_registry", "l2_output_oracle"])]
    pub network: Option<NetworkPreset>,
    /// Custom L2 RPC endpoint.
    #[arg(long, requires = "portal", value_parser = RpcUrlValueParser)]
    pub l2_rpc_url: Option<Url>,
    /// Custom Optimism portal.
    #[arg(long, requires_all = ["l2_rpc_url", "proofs"])]
    pub portal: Option<Address>,
    /// Expected custom dispute game factory, checked against the portal.
    #[arg(long, requires_all = ["portal", "anchor_state_registry"])]
    pub dispute_game_factory: Option<Address>,
    /// Expected custom anchor state registry, checked against the portal.
    #[arg(long, requires = "dispute_game_factory", conflicts_with = "l2_output_oracle")]
    pub anchor_state_registry: Option<Address>,
    /// Custom legacy L2 output oracle.
    #[arg(long, requires = "portal")]
    pub l2_output_oracle: Option<Address>,
    /// Hex-encoded 32-byte private key.
    #[arg(long, env = "WITHDRAWER_PRIVATE_KEY", hide_env_values = true, group = "signer", allow_hyphen_values = true, value_parser = SecretValueParser::<B256>::default())]
    pub private_key: Option<Secret<B256>>,
    /// English BIP-39 mnemonic phrase.
    #[arg(long, env = "WITHDRAWER_MNEMONIC", hide_env_values = true, group = "signer", num_args = 1.., value_parser = SecretValueParser::<String>::default())]
    pub mnemonic: Vec<Secret<String>>,
    /// Use a Ledger device.
    #[cfg(feature = "ledger")]
    #[arg(long, group = "signer")]
    pub ledger: bool,
    /// Mnemonic or Ledger derivation path.
    #[arg(long, default_value = "m/44'/60'/0'/0/0", conflicts_with = "private_key", value_parser = HdPathValueParser)]
    pub hd_path: DerivationPath,
    /// Minimum gas limit; gas is always estimated, so this cannot bypass estimation failures.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub gas_limit: Option<u64>,
    /// Transaction management configuration.
    #[command(flatten)]
    pub tx_manager: TxManagerCli,
    /// Simulate without signing or submission.
    #[arg(long)]
    pub dry_run: bool,
    /// Overall operation timeout.
    #[arg(long, default_value = "10m", value_parser = humantime::parse_duration)]
    pub timeout: Duration,
    /// HTTP retries after the first attempt, at most 32; submission RPCs are also bounded by --tx-manager.network-timeout.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(..=32))]
    pub max_retries: u32,
    /// Initial HTTP retry backoff, subject to the operation and submission RPC deadlines.
    #[arg(long, default_value = "2s", value_parser = humantime::parse_duration)]
    pub retry_delay: Duration,
}

impl WithdrawerArgs {
    /// Parses arguments with safe flag names and reasons, without echoing rejected values.
    pub fn parse_matches<I, T>(command: Command, itr: I) -> Result<ArgMatches, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        command.try_get_matches_from(itr).map_err(|error| {
            if error.kind() == ErrorKind::InvalidValue
                && matches!(
                    error.get(ContextKind::InvalidValue),
                    Some(ContextValue::String(value)) if value.is_empty()
                )
            {
                return error;
            }
            // For these errors, InvalidArg is the declared argument, not the rejected value.
            if matches!(error.kind(), ErrorKind::InvalidValue | ErrorKind::ValueValidation)
                && let Some(arg) = error.get(ContextKind::InvalidArg)
            {
                return clap::Error::raw(
                    error.kind(),
                    format!("invalid value for '{arg}'; the value is redacted"),
                );
            }
            // Redacting value parsers return raw errors without rejected-value context.
            if matches!(
                error.kind(),
                ErrorKind::UnknownArgument
                    | ErrorKind::InvalidSubcommand
                    | ErrorKind::InvalidValue
                    | ErrorKind::TooManyValues
                    | ErrorKind::InvalidUtf8
            ) || (error.kind() == ErrorKind::ValueValidation
                && error.get(ContextKind::InvalidValue).is_some())
            {
                clap::Error::raw(
                    error.kind(),
                    "invalid arguments; potentially sensitive values are redacted",
                )
            } else {
                error
            }
        })
    }

    /// Parses a BIP-32 path with 31-bit component indices and at most 255 components.
    pub fn parse_hd_path(value: &str) -> Result<DerivationPath, &'static str> {
        for (depth, component) in value.split('/').filter(|component| *component != "m").enumerate()
        {
            if depth >= SignerSource::MAX_DERIVATION_DEPTH {
                return Err("derivation path must have at most 255 components");
            }
            let index = component.strip_suffix(['\'', 'h']).unwrap_or(component);
            if index.parse::<u32>().map_or(true, |index| index >= coins_bip32::BIP32_HARDEN) {
                return Err("derivation path indices must be between 0 and 2147483647");
            }
        }
        value.parse().map_err(|_| "invalid derivation path")
    }

    /// Resolves either a preset or one complete custom proof system.
    pub fn resolve_network(&self) -> Result<Network, ConfigError> {
        if [
            self.portal,
            self.dispute_game_factory,
            self.anchor_state_registry,
            self.l2_output_oracle,
        ]
        .into_iter()
        .flatten()
        .any(|address| address == Address::ZERO)
        {
            return Err(ConfigError::ZeroAddress);
        }
        match (
            self.network,
            &self.l2_rpc_url,
            self.portal,
            self.dispute_game_factory,
            self.anchor_state_registry,
            self.l2_output_oracle,
        ) {
            (preset, None, None, None, None, None) => Ok(preset.unwrap_or_default().resolve()),
            (None, Some(url), Some(portal), Some(factory), Some(registry), None) => Ok(Network {
                l2_rpc_url: url.clone(),
                portal,
                proofs: ProofSystem::FaultProofs {
                    expected: Some(FaultProofContracts {
                        dispute_game_factory: factory,
                        anchor_state_registry: registry,
                    }),
                },
                l1_chain_id: None,
            }),
            (None, Some(url), Some(portal), None, None, Some(oracle)) => Ok(Network {
                l2_rpc_url: url.clone(),
                portal,
                proofs: ProofSystem::OutputOracle { l2_output_oracle: oracle },
                l1_chain_id: None,
            }),
            _ => Err(ConfigError::Network),
        }
    }
}

impl CommandFactory for WithdrawerArgs {
    fn command() -> Command {
        Self::augment_args(Command::new("withdrawer").version(env!("CARGO_PKG_VERSION")))
            .about(WITHDRAWER_ABOUT)
    }

    fn command_for_update() -> Command {
        Self::augment_args_for_update(Command::new("withdrawer").version(env!("CARGO_PKG_VERSION")))
            .about(WITHDRAWER_ABOUT)
    }
}

// clap can echo stray secrets and RPC URLs before any value parser can redact them.
impl Parser for WithdrawerArgs {
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
        let mut matches = Self::parse_matches(Self::command(), itr)?;
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
        let mut matches = Self::parse_matches(Self::command_for_update(), itr)?;
        self.update_from_arg_matches_mut(&mut matches)
    }
}

/// Selected signing material without key derivation or device access.
#[derive(Debug, Clone)]
pub enum SignerSource {
    /// Raw 32-byte private key.
    PrivateKey(Secret<B256>),
    /// Validated English mnemonic and derivation path.
    Mnemonic {
        /// Mnemonic phrase.
        phrase: Secret<Mnemonic<English>>,
        /// BIP-32 derivation path.
        path: DerivationPath,
    },
    /// Ledger device and derivation path.
    #[cfg(feature = "ledger")]
    Ledger {
        /// BIP-32 derivation path.
        path: DerivationPath,
    },
}

impl SignerSource {
    /// Maximum BIP-32 path depth, bounded by the one-byte depth field of extended keys.
    pub const MAX_DERIVATION_DEPTH: usize = u8::MAX as usize;
}

/// Fully resolved operation settings, validated without RPC.
#[derive(Debug, Clone)]
pub struct WithdrawerConfig {
    /// L1 RPC endpoint.
    pub l1_rpc_url: Url,
    /// L2 endpoint, portal, and proof-system expectations.
    pub network: Network,
    /// L2 withdrawal transaction hash.
    pub withdrawal: B256,
    /// Selected signing source.
    pub signer: SignerSource,
    /// Minimum gas limit; gas is always estimated, so this cannot bypass estimation failures.
    pub gas_limit: Option<u64>,
    /// Fee bumping, resubmission and confirmation policy.
    pub tx_manager: base_tx_manager::TxManagerConfig,
    /// Disable signing and submission.
    pub dry_run: bool,
    /// Overall timeout.
    pub timeout: Duration,
    /// HTTP retry limit, subject to the operation and transaction-manager network timeouts.
    pub max_retries: u32,
    /// Initial HTTP retry backoff, subject to the operation and submission RPC deadlines.
    pub retry_delay: Duration,
}

impl WithdrawerConfig {
    /// Bounds submission HTTP attempts so a retry and its backoff fit the network deadline.
    pub fn submission_request_timeout(&self) -> Result<Duration, ConfigError> {
        let network_budget = if self.max_retries > 0 {
            self.tx_manager.network_timeout.saturating_sub(self.retry_delay) / 3
        } else {
            self.tx_manager.network_timeout / 2
        };
        if network_budget.is_zero() {
            return Err(ConfigError::RetryDeadline);
        }
        // Reserve time beyond two attempts for scheduling and response processing.
        Ok(crate::WithdrawalRpcTransport::REQUEST_TIMEOUT.min(self.timeout / 2).min(network_budget))
    }
}

/// Invalid cross-field operation settings.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Signer selection is missing or ambiguous.
    #[error("exactly one signing source is required")]
    Signer,
    /// Custom network contracts are incomplete, mixed, or combined with a preset.
    #[error("supply one complete custom network without --network, or select a preset")]
    Network,
    /// A custom contract address is zero.
    #[error("custom contract addresses must not be zero")]
    ZeroAddress,
    /// The mnemonic is not a valid English BIP-39 phrase.
    #[error("invalid signing secret; check its format")]
    Mnemonic,
    /// Explicit gas limit is zero.
    #[error("--gas-limit must be positive")]
    GasLimit,
    /// Transaction manager configuration is invalid.
    #[error("invalid transaction manager config: {0}")]
    TxManager(#[from] base_tx_manager::ConfigError),
    /// Timeout or retry delay is zero.
    #[error("--timeout and --retry-delay must be positive")]
    ZeroDuration,
    /// Retry count exceeds the supported bound.
    #[error("--max-retries must be at most 32")]
    Retries,
    /// The network deadline leaves no time for bounded submission attempts.
    #[error(
        "--tx-manager.network-timeout must leave time for HTTP attempts after --retry-delay when retries are enabled"
    )]
    RetryDeadline,
    /// Exponential retry delay does not fit a Rust duration.
    #[error("exponential retry delay overflows Duration")]
    RetryOverflow,
}

impl TryFrom<WithdrawerArgs> for WithdrawerConfig {
    type Error = ConfigError;

    fn try_from(args: WithdrawerArgs) -> Result<Self, Self::Error> {
        let network = args.resolve_network()?;
        if args.gas_limit == Some(0) {
            return Err(ConfigError::GasLimit);
        }
        if args.timeout.is_zero() || args.retry_delay.is_zero() {
            return Err(ConfigError::ZeroDuration);
        }
        if args.max_retries > 32 {
            return Err(ConfigError::Retries);
        }
        if args.max_retries > 0
            && args.retry_delay.checked_mul(1u32 << (args.max_retries - 1)).is_none()
        {
            return Err(ConfigError::RetryOverflow);
        }
        let tx_manager = base_tx_manager::TxManagerConfig::try_from(args.tx_manager.clone())?;
        let signer_count =
            usize::from(args.private_key.is_some()) + usize::from(!args.mnemonic.is_empty());
        #[cfg(feature = "ledger")]
        let signer_count = signer_count + usize::from(args.ledger);
        if signer_count != 1 {
            return Err(ConfigError::Signer);
        }
        let signer = match (args.private_key, args.mnemonic.is_empty()) {
            (Some(key), true) => SignerSource::PrivateKey(key),
            (None, false) => {
                let phrase = Secret(
                    args.mnemonic.iter().map(|word| word.0.as_str()).collect::<Vec<_>>().join(" "),
                );
                let phrase = phrase.0.parse().map(Secret).map_err(|_| ConfigError::Mnemonic)?;
                SignerSource::Mnemonic { phrase, path: args.hd_path }
            }
            #[cfg(feature = "ledger")]
            (None, true) if args.ledger => SignerSource::Ledger { path: args.hd_path },
            _ => return Err(ConfigError::Signer),
        };
        let config = Self {
            l1_rpc_url: args.l1_rpc_url,
            network,
            withdrawal: args.withdrawal,
            signer,
            gas_limit: args.gas_limit,
            tx_manager,
            dry_run: args.dry_run,
            timeout: args.timeout,
            max_retries: args.max_retries,
            retry_delay: args.retry_delay,
        };
        config.submission_request_timeout()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use std::{env, process::Command, time::Duration};

    use clap::{Parser, error::ErrorKind};

    use crate::{NetworkPreset, ProofSystem, SignerSource, WithdrawerArgs, WithdrawerConfig};

    const HASH: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const KEY: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const PHRASE: &str = "test test test test test test test test test test test junk";
    const ADDRESS: &str = "0x1111111111111111111111111111111111111111";

    fn base() -> Vec<String> {
        vec![
            "withdrawer".into(),
            "--l1-rpc-url=https://l1.example".into(),
            format!("--withdrawal={HASH}"),
        ]
    }

    fn args(extra: &[&str]) -> Result<WithdrawerArgs, clap::Error> {
        WithdrawerArgs::try_parse_from(base().into_iter().chain(extra.iter().map(|s| (*s).into())))
    }

    fn config(extra: &[&str]) -> Result<WithdrawerConfig, String> {
        args(extra)
            .map_err(|e| e.to_string())?
            .try_into()
            .map_err(|e: crate::ConfigError| e.to_string())
    }

    fn signed(extra: &[&str]) -> Result<WithdrawerConfig, String> {
        let key = format!("--private-key={KEY}");
        config(&[key.as_str()].into_iter().chain(extra.iter().copied()).collect::<Vec<_>>())
    }

    fn custom(oracle: bool) -> Vec<String> {
        let mut flags =
            vec!["--l2-rpc-url=https://l2.example".into(), format!("--portal={ADDRESS}")];
        if oracle {
            flags.push(format!("--l2-output-oracle={ADDRESS}"));
        } else {
            flags.extend([
                format!("--dispute-game-factory={ADDRESS}"),
                format!("--anchor-state-registry={ADDRESS}"),
            ]);
        }
        flags
    }

    #[test]
    fn rust_cli_syntax_durations_and_retry_boundaries() {
        let defaults = signed(&[]).unwrap();
        assert_eq!(defaults.timeout, Duration::from_secs(600));
        assert_eq!(defaults.max_retries, 3);
        assert_eq!(defaults.retry_delay, Duration::from_secs(2));
        for flag in [
            "--retry-delay=0s",
            "--timeout=0s",
            "--max-retries=33",
            "--max-retries=-1",
            "--gas-limit=0",
            "--dry-run=TRUE",
            "--gas-limit=0x100",
        ] {
            assert!(signed(&[flag]).is_err(), "accepted {flag}");
        }
        assert_eq!(signed(&["--timeout=1h 30m"]).unwrap().timeout, Duration::from_secs(5400));
        assert!(signed(&["--max-retries=0"]).is_ok());
        assert!(signed(&["--max-retries=32", "--retry-delay=1ns"]).is_ok());
        let long_delay = format!("--retry-delay={}s", u64::MAX);
        assert!(signed(&["--max-retries=1", &long_delay]).is_err());
        assert!(signed(&["--max-retries=2", &long_delay]).is_err());
        assert!(signed(&["--max-retries=0", &long_delay]).is_ok());
    }

    #[test]
    fn submission_retries_require_time_after_the_backoff() {
        for delay in ["10s", "11s"] {
            let error = signed(&[&format!("--retry-delay={delay}")]).unwrap_err();
            assert!(error.contains("--tx-manager.network-timeout"));
            assert!(error.contains("--retry-delay"));
            assert!(signed(&["--max-retries=0", &format!("--retry-delay={delay}")]).is_ok());
        }
    }

    #[test]
    fn selected_presets_and_derivation_paths_are_resolved_before_rpc() {
        for (preset, url) in [
            ("base-mainnet", "https://mainnet.base.org/"),
            ("base-sepolia", "https://sepolia.base.org/"),
            ("op-mainnet", "https://mainnet.optimism.io/"),
            ("op-sepolia", "https://sepolia.optimism.io/"),
        ] {
            let network = signed(&[&format!("--network={preset}")]).unwrap().network;
            assert_eq!(network.l2_rpc_url.as_str(), url);
            assert!(matches!(network.proofs, ProofSystem::FaultProofs { .. }));
        }
        assert!(signed(&["--network=unknown"]).is_err());
        assert!(
            WithdrawerArgs::try_parse_from([
                "withdrawer",
                "--l1-rpc-url=invalid",
                &format!("--withdrawal={HASH}"),
                &format!("--private-key={KEY}")
            ])
            .is_err()
        );
        let mnemonic = format!("--mnemonic={PHRASE}");
        let SignerSource::Mnemonic { path, .. } = config(&[&mnemonic]).unwrap().signer else {
            panic!("expected mnemonic")
        };
        assert_eq!(path.derivation_string(), "m/44'/60'/0'/0/0");
        assert!(config(&[&mnemonic, "--hd-path=not-a-path"]).is_err());
        let SignerSource::Mnemonic { path, .. } =
            config(&[&mnemonic, "--hd-path=m/44'/60'/1'/0/2"]).unwrap().signer
        else {
            panic!("expected mnemonic")
        };
        assert_eq!(path.derivation_string(), "m/44'/60'/1'/0/2");
    }

    #[test]
    fn derivation_path_indices_preserve_hardening_at_the_boundary() {
        let mnemonic = format!("--mnemonic={PHRASE}");
        for suffix in ["", "'", "h"] {
            let valid = format!("--hd-path=m/2147483647{suffix}");
            let SignerSource::Mnemonic { path, .. } = config(&[&mnemonic, &valid]).unwrap().signer
            else {
                panic!("expected mnemonic")
            };
            let expected = if suffix.is_empty() { "m/2147483647" } else { "m/2147483647'" };
            assert_eq!(path.derivation_string(), expected);
            for index in ["2147483648", "4294967295", "4294967296"] {
                let invalid = format!("--hd-path=m/{index}{suffix}");
                assert_eq!(
                    args(&[&mnemonic, &invalid]).unwrap_err().kind(),
                    ErrorKind::ValueValidation
                );
            }
        }
    }

    #[test]
    fn invalid_rpc_urls_do_not_echo_credentials() {
        for flag in ["--l1-rpc-url", "--l2-rpc-url"] {
            let mut argv = base();
            if flag == "--l1-rpc-url" {
                argv.remove(1);
            }
            let value = "https://user:example-password@";
            argv.extend([flag.into(), value.into()]);
            let error = WithdrawerArgs::try_parse_from(argv).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ValueValidation);
            let diagnostic = format!("{error} {error:?}");
            assert!(diagnostic.contains("invalid RPC URL"));
            assert!(diagnostic.contains(flag));
            assert!(diagnostic.contains("empty host"));
            assert!(!diagnostic.contains(value));
            assert!(!diagnostic.contains("example-password"), "credentials leaked: {diagnostic}");
        }
    }

    #[test]
    fn invalid_values_name_flags_without_echoing_values() {
        for (flag, value, kind, reason) in [
            ("--network", "base-mainet", ErrorKind::InvalidValue, None),
            ("--timeout", "bogus", ErrorKind::ValueValidation, None),
            ("--private-key", "private-secret-not-hex", ErrorKind::ValueValidation, None),
            (
                "--hd-path",
                "m/2147483648",
                ErrorKind::ValueValidation,
                Some("derivation path indices must be between 0 and 2147483647"),
            ),
        ] {
            let error = args(&[flag, value]).unwrap_err();
            assert_eq!(error.kind(), kind);
            let diagnostic = format!("{error} {error:?}");
            assert!(diagnostic.contains(flag), "missing flag: {diagnostic}");
            assert!(!diagnostic.contains(value), "rejected value leaked: {diagnostic}");
            if let Some(reason) = reason {
                assert!(diagnostic.contains(reason), "missing reason: {diagnostic}");
            }
        }

        let error = WithdrawerArgs::try_parse_from([
            "withdrawer",
            "--withdrawal",
            HASH,
            "--private-key",
            KEY,
            "--l1-rpc-url",
        ])
        .unwrap_err();
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("a value is required for '--l1-rpc-url <L1_RPC_URL>'"));
        assert!(!diagnostic.contains("redacted"));
    }

    #[test]
    fn stray_rpc_urls_do_not_echo_credentials_without_signing_flags() {
        let error = args(&["https://user:example-password@l2.example"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
        let diagnostic = format!("{error} {error:?}");
        assert!(diagnostic.contains("potentially sensitive values are redacted"));
        for credential in ["user", "example-password"] {
            assert!(!diagnostic.contains(credential), "credentials leaked: {diagnostic}");
        }
    }

    #[test]
    fn secrets_can_come_from_the_environment() {
        if env::var_os("WITHDRAWER_TEST_CHILD").is_some() {
            let parsed = args(&[]).unwrap();
            let debug = format!("{parsed:?}");
            assert!(!debug.contains(KEY) && !debug.contains(PHRASE));
            let config = WithdrawerConfig::try_from(parsed).unwrap();
            assert!(matches!(
                config.signer,
                SignerSource::PrivateKey(_) | SignerSource::Mnemonic { .. }
            ));
            let help = args(&["--help"]).unwrap_err().to_string();
            assert!(!help.contains(KEY) && !help.contains(PHRASE));
            return;
        }
        for (name, value) in [("WITHDRAWER_PRIVATE_KEY", KEY), ("WITHDRAWER_MNEMONIC", PHRASE)] {
            let output = Command::new(env::current_exe().unwrap())
                .args(["--exact", "config::tests::secrets_can_come_from_the_environment"])
                .env_remove("WITHDRAWER_PRIVATE_KEY")
                .env_remove("WITHDRAWER_MNEMONIC")
                .env("WITHDRAWER_TEST_CHILD", "1")
                .env(name, value)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "environment signer failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn malformed_hashes_and_invalid_custom_addresses_are_rejected() {
        assert!(signed(&[]).is_ok());
        for bad in [
            "0x0000000000000000000000000000000000000000",
            "0xgg",
            "0x1",
            "0x111111111111111111111111111111111111111111",
            "0xgg11111111111111111111111111111111111111",
        ] {
            for oracle in [false, true] {
                let flags = custom(oracle);
                for i in 1..flags.len() {
                    let mut invalid = flags.clone();
                    let name = invalid[i].split('=').next().unwrap();
                    invalid[i] = format!("{name}={bad}");
                    assert!(
                        signed(&invalid.iter().map(String::as_str).collect::<Vec<_>>()).is_err(),
                        "accepted invalid address {}",
                        invalid[i]
                    );
                }
            }
        }
        for bad in [
            "0xgg",
            "0x1",
            ADDRESS,
            "0xggaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            let argv = [
                "withdrawer",
                "--l1-rpc-url=https://l1.example",
                &format!("--private-key={KEY}"),
                &format!("--withdrawal={bad}"),
            ];
            assert!(
                WithdrawerArgs::try_parse_from(argv).is_err(),
                "accepted invalid withdrawal {bad}"
            );
        }
    }

    #[test]
    fn exactly_one_signer_is_required() {
        assert!(config(&[&format!("--private-key={KEY}")]).is_ok());
        assert!(matches!(
            config(&[&format!("--mnemonic={PHRASE}")]).unwrap().signer,
            SignerSource::Mnemonic { .. }
        ));
        assert!(config(&[]).is_err(), "accepted missing signer");
        assert!(
            config(&[&format!("--private-key={KEY}"), &format!("--mnemonic={PHRASE}")]).is_err(),
            "accepted two signers"
        );
        #[cfg(feature = "ledger")]
        {
            assert!(matches!(config(&["--ledger"]).unwrap().signer, SignerSource::Ledger { .. }));
            for other in [format!("--private-key={KEY}"), format!("--mnemonic={PHRASE}")] {
                assert!(config(&["--ledger", &other]).is_err());
            }
        }
        #[cfg(not(feature = "ledger"))]
        assert!(args(&["--ledger"]).is_err());
    }

    #[test]
    fn custom_network_requires_one_complete_proof_system_without_a_preset() {
        assert_eq!(signed(&[]).unwrap().network, NetworkPreset::BaseMainnet.resolve());
        for oracle in [false, true] {
            let flags = custom(oracle);
            let refs = flags.iter().map(String::as_str).collect::<Vec<_>>();
            let network = signed(&refs).unwrap().network;
            assert_eq!(network.l2_rpc_url.as_str(), "https://l2.example/");
            assert!(
                matches!(
                    (oracle, network.proofs),
                    (true, ProofSystem::OutputOracle { .. })
                        | (false, ProofSystem::FaultProofs { .. })
                ),
                "incorrect custom proof system"
            );
            for mask in 1..(1 << refs.len()) - 1 {
                let partial = refs
                    .iter()
                    .enumerate()
                    .filter_map(|(j, f)| (mask & (1 << j) != 0).then_some(*f))
                    .collect::<Vec<_>>();
                assert!(signed(&partial).is_err(), "accepted partial custom network {partial:?}");
            }
            assert!(signed(&[refs.as_slice(), &["--network=base-mainnet"]].concat()).is_err());
        }
        let mut mixed = custom(false);
        mixed.push(format!("--l2-output-oracle={ADDRESS}"));
        assert!(signed(&mixed.iter().map(String::as_str).collect::<Vec<_>>()).is_err());
    }

    #[test]
    fn secrets_are_redacted_in_args_config_signer_and_parse_errors() {
        for flag in [format!("--private-key={KEY}"), format!("--mnemonic={PHRASE}")] {
            let parsed = args(&[&flag]).unwrap();
            let debug_args = format!("{parsed:?}");
            let config = WithdrawerConfig::try_from(parsed).unwrap();
            for debug in [debug_args, format!("{config:?}"), format!("{:?}", config.signer)] {
                assert!(
                    !debug.contains(KEY) && !debug.contains(PHRASE),
                    "debug exposed signing secret"
                );
                assert!(debug.contains("[REDACTED]"));
            }
        }
        for flags in [
            vec!["--private-key=private-secret-not-hex"],
            vec!["--mnemonic=alpha bravo charlie"],
            vec!["--mnemonic", "alpha", "bravo", "charlie"],
        ] {
            let error = config(&flags).unwrap_err();
            for word in ["private-secret-not-hex", "alpha", "bravo", "charlie"] {
                assert!(!error.contains(word), "parse error exposed signing secret");
            }
        }
        for name in ["--private-key", "--mnemonic"] {
            let error = args(&[name, "--secret-leading-hyphens"]).unwrap_err().to_string();
            assert!(
                !error.contains("--secret-leading-hyphens"),
                "parse error exposed a hyphenated secret"
            );
        }
        for (flags, kind) in [
            (vec!["--mnemonic", "alpha", "bravo", "--charlie"], ErrorKind::UnknownArgument),
            (vec!["--mnemonic=alpha", "bravo", "--charlie"], ErrorKind::UnknownArgument),
            (vec!["--mnemonic", "alpha", "--network=bravo"], ErrorKind::InvalidValue),
            (vec!["--private-key", KEY, "bravo"], ErrorKind::UnknownArgument),
            (vec![&format!("--private-key={KEY}"), "bravo"], ErrorKind::UnknownArgument),
        ] {
            let error = args(&flags).unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(error.exit_code(), 2);
            for word in ["alpha", "bravo", "charlie", KEY] {
                assert!(!error.to_string().contains(word), "parse error exposed signing secret");
            }
        }
        let unquoted = ["--mnemonic"]
            .into_iter()
            .chain(PHRASE.split_whitespace())
            .chain(["--hd-path=m/1"])
            .collect::<Vec<_>>();
        let SignerSource::Mnemonic { path, .. } = config(&unquoted).unwrap().signer else {
            panic!("expected mnemonic")
        };
        assert_eq!(path.derivation_string(), "m/1");
        let unknown = args(&["--unknown-without-secret"]).unwrap_err().to_string();
        assert!(unknown.contains("potentially sensitive values are redacted"));
        let conflict = args(&[&format!("--private-key={KEY}"), &format!("--mnemonic={PHRASE}")])
            .unwrap_err()
            .to_string();
        assert!(!conflict.contains(KEY) && !conflict.contains(PHRASE));
    }
}
