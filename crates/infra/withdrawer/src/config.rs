use std::time::Duration;

use alloy_primitives::{Address, B256};
use clap::{ArgGroup, Parser};
use coins_bip32::path::DerivationPath;
use coins_bip39::{English, Mnemonic};
use thiserror::Error;
use url::Url;

use crate::{Network, NetworkPreset, ProofSystem, Secret, SecretValueParser};

/// Command-line withdrawal settings before cross-field validation.
#[derive(Debug, Clone, Parser)]
#[command(name = "withdrawer")]
#[command(group(ArgGroup::new("signer").required(true).multiple(false)))]
#[command(group(ArgGroup::new("proofs").args(["dispute_game_factory", "l2_output_oracle"])))]
pub struct WithdrawerArgs {
    /// L1 RPC endpoint.
    #[arg(long)]
    pub l1_rpc_url: Url,
    /// L2 withdrawal transaction hash.
    #[arg(long)]
    pub withdrawal: B256,
    /// Built-in network, defaulting to base-mainnet without custom contracts.
    #[arg(long, value_enum, conflicts_with_all = ["l2_rpc_url", "portal", "dispute_game_factory", "anchor_state_registry", "l2_output_oracle"])]
    pub network: Option<NetworkPreset>,
    /// Custom L2 RPC endpoint.
    #[arg(long, requires = "portal")]
    pub l2_rpc_url: Option<Url>,
    /// Custom Optimism portal.
    #[arg(long, requires_all = ["l2_rpc_url", "proofs"])]
    pub portal: Option<Address>,
    /// Custom dispute game factory.
    #[arg(long, requires_all = ["portal", "anchor_state_registry"])]
    pub dispute_game_factory: Option<Address>,
    /// Custom anchor state registry.
    #[arg(long, requires = "dispute_game_factory", conflicts_with = "l2_output_oracle")]
    pub anchor_state_registry: Option<Address>,
    /// Custom legacy L2 output oracle.
    #[arg(long, requires = "portal")]
    pub l2_output_oracle: Option<Address>,
    /// Hex-encoded 32-byte private key.
    #[arg(long, env = "WITHDRAWER_PRIVATE_KEY", hide_env_values = true, group = "signer", allow_hyphen_values = true, value_parser = SecretValueParser::<B256>::default())]
    pub private_key: Option<Secret<B256>>,
    /// English BIP-39 mnemonic phrase.
    #[arg(long, env = "WITHDRAWER_MNEMONIC", hide_env_values = true, group = "signer", allow_hyphen_values = true, value_parser = SecretValueParser::<Mnemonic<English>>::default())]
    pub mnemonic: Option<Secret<Mnemonic<English>>>,
    /// Use a Ledger device.
    #[cfg(feature = "ledger")]
    #[arg(long, group = "signer")]
    pub ledger: bool,
    /// Mnemonic or Ledger derivation path.
    #[arg(long, default_value = "m/44'/60'/0'/0/0", conflicts_with = "private_key")]
    pub hd_path: DerivationPath,
    /// Explicit gas limit, bypassing estimation.
    #[arg(long, conflicts_with = "gas_multiplier", value_parser = clap::value_parser!(u64).range(1..))]
    pub gas_limit: Option<u64>,
    /// Legacy gas price in wei.
    #[arg(long, conflicts_with_all = ["max_fee_per_gas", "max_priority_fee_per_gas"])]
    pub gas_price: Option<u128>,
    /// EIP-1559 maximum fee in wei.
    #[arg(long, requires = "max_priority_fee_per_gas")]
    pub max_fee_per_gas: Option<u128>,
    /// EIP-1559 priority fee in wei.
    #[arg(long, requires = "max_fee_per_gas")]
    pub max_priority_fee_per_gas: Option<u128>,
    /// Gas estimation multiplier, finite and at least 1.0.
    #[arg(long, default_value_t = 1.0)]
    pub gas_multiplier: f64,
    /// Safety cap for explicit or estimated per-gas fees in wei.
    #[arg(long)]
    pub max_gas_price: Option<u128>,
    /// Simulate without signing or submission.
    #[arg(long)]
    pub dry_run: bool,
    /// Overall operation timeout.
    #[arg(long, default_value = "10m", value_parser = humantime::parse_duration)]
    pub timeout: Duration,
    /// Retries after the first attempt, at most 32.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(..=32))]
    pub max_retries: u32,
    /// Initial exponential retry delay.
    #[arg(long, default_value = "2s", value_parser = humantime::parse_duration)]
    pub retry_delay: Duration,
}

impl WithdrawerArgs {
    /// Resolves either a preset or one complete custom proof system.
    pub fn resolve_network(&self) -> Result<Network, ConfigError> {
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
                    dispute_game_factory: factory,
                    anchor_state_registry: registry,
                },
            }),
            (None, Some(url), Some(portal), None, None, Some(oracle)) => Ok(Network {
                l2_rpc_url: url.clone(),
                portal,
                proofs: ProofSystem::OutputOracle { l2_output_oracle: oracle },
            }),
            _ => Err(ConfigError::Network),
        }
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

/// Transaction fee selection in wei per gas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fees {
    /// Estimate fees at submission time.
    Automatic,
    /// Legacy transaction.
    Legacy {
        /// Gas price.
        gas_price: u128,
    },
    /// EIP-1559 transaction.
    Eip1559 {
        /// Maximum total fee.
        max_fee_per_gas: u128,
        /// Maximum priority fee.
        max_priority_fee_per_gas: u128,
    },
}

/// Fully resolved operation settings, validated without RPC.
#[derive(Debug, Clone)]
pub struct WithdrawerConfig {
    /// L1 RPC endpoint.
    pub l1_rpc_url: Url,
    /// L2 endpoint and L1 contracts.
    pub network: Network,
    /// L2 withdrawal transaction hash.
    pub withdrawal: B256,
    /// Selected signing source.
    pub signer: SignerSource,
    /// Explicit gas limit.
    pub gas_limit: Option<u64>,
    /// Transaction fee selection.
    pub fees: Fees,
    /// Gas estimation multiplier.
    pub gas_multiplier: f64,
    /// Safety cap for explicit or estimated per-gas fees.
    pub max_gas_price: Option<u128>,
    /// Disable signing and submission.
    pub dry_run: bool,
    /// Overall timeout.
    pub timeout: Duration,
    /// Retries after the first attempt.
    pub max_retries: u32,
    /// Initial exponential retry delay.
    pub retry_delay: Duration,
}

/// Invalid cross-field operation settings.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    /// Signer selection is missing or ambiguous.
    #[error("exactly one signing source is required")]
    Signer,
    /// Custom network contracts are incomplete, mixed, or combined with a preset.
    #[error("supply one complete custom network without --network, or select a preset")]
    Network,
    /// Gas estimation multiplier is not finite or is below one.
    #[error("--gas-multiplier must be finite and at least 1.0")]
    GasMultiplier,
    /// Explicit gas limit conflicts with a non-default multiplier or is zero.
    #[error("--gas-limit must be positive and cannot be combined with --gas-multiplier")]
    GasLimit,
    /// Fee selection mixes modes or omits one EIP-1559 fee.
    #[error("select legacy --gas-price or both EIP-1559 fees")]
    Fees,
    /// Priority fee exceeds the total fee.
    #[error("--max-priority-fee-per-gas must not exceed --max-fee-per-gas")]
    PriorityFee,
    /// An explicit fee exceeds the safety cap.
    #[error("explicit fee exceeds --max-gas-price")]
    FeeCap,
    /// Timeout or retry delay is zero.
    #[error("--timeout and --retry-delay must be positive")]
    ZeroDuration,
    /// Retry count exceeds the supported bound.
    #[error("--max-retries must be at most 32")]
    Retries,
    /// Exponential retry delay does not fit a Rust duration.
    #[error("exponential retry delay overflows Duration")]
    RetryOverflow,
}

impl TryFrom<WithdrawerArgs> for WithdrawerConfig {
    type Error = ConfigError;

    fn try_from(args: WithdrawerArgs) -> Result<Self, Self::Error> {
        let network = args.resolve_network()?;
        if !args.gas_multiplier.is_finite() || args.gas_multiplier < 1.0 {
            return Err(ConfigError::GasMultiplier);
        }
        if args.gas_limit.is_some_and(|limit| limit == 0 || args.gas_multiplier != 1.0) {
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
        let fees = match (args.gas_price, args.max_fee_per_gas, args.max_priority_fee_per_gas) {
            (None, None, None) => Fees::Automatic,
            (Some(gas_price), None, None) => Fees::Legacy { gas_price },
            (None, Some(max_fee_per_gas), Some(max_priority_fee_per_gas)) => {
                if max_priority_fee_per_gas > max_fee_per_gas {
                    return Err(ConfigError::PriorityFee);
                }
                Fees::Eip1559 { max_fee_per_gas, max_priority_fee_per_gas }
            }
            _ => return Err(ConfigError::Fees),
        };
        if let Some(cap) = args.max_gas_price
            && [args.gas_price, args.max_fee_per_gas, args.max_priority_fee_per_gas]
                .into_iter()
                .flatten()
                .any(|fee| fee > cap)
        {
            return Err(ConfigError::FeeCap);
        }
        let signer_count =
            usize::from(args.private_key.is_some()) + usize::from(args.mnemonic.is_some());
        #[cfg(feature = "ledger")]
        let signer_count = signer_count + usize::from(args.ledger);
        if signer_count != 1 {
            return Err(ConfigError::Signer);
        }
        let signer = match (args.private_key, args.mnemonic) {
            (Some(key), None) => SignerSource::PrivateKey(key),
            (None, Some(phrase)) => SignerSource::Mnemonic { phrase, path: args.hd_path },
            #[cfg(feature = "ledger")]
            (None, None) if args.ledger => SignerSource::Ledger { path: args.hd_path },
            _ => return Err(ConfigError::Signer),
        };
        Ok(Self {
            l1_rpc_url: args.l1_rpc_url,
            network,
            withdrawal: args.withdrawal,
            signer,
            gas_limit: args.gas_limit,
            fees,
            gas_multiplier: args.gas_multiplier,
            max_gas_price: args.max_gas_price,
            dry_run: args.dry_run,
            timeout: args.timeout,
            max_retries: args.max_retries,
            retry_delay: args.retry_delay,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{env, process::Command, time::Duration};

    use clap::Parser;

    use crate::{Fees, NetworkPreset, ProofSystem, SignerSource, WithdrawerArgs, WithdrawerConfig};

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
            "-gas-price=1",
            "--dry-run=TRUE",
            "--gas-limit=0x100",
            "--gas-multiplier=0x1p0",
        ] {
            assert!(signed(&[flag]).is_err(), "accepted {flag}");
        }
        assert_eq!(signed(&["--timeout=1h 30m"]).unwrap().timeout, Duration::from_secs(5400));
        assert!(signed(&["--max-retries=0"]).is_ok());
        assert!(signed(&["--max-retries=32", "--retry-delay=1ns"]).is_ok());
        let long_delay = format!("--retry-delay={}s", u64::MAX);
        assert!(signed(&["--max-retries=1", &long_delay]).is_ok());
        assert!(signed(&["--max-retries=2", &long_delay]).is_err());
        assert!(signed(&["--max-retries=0", &long_delay]).is_ok());
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
    fn malformed_hashes_and_every_custom_address_are_rejected() {
        assert!(signed(&[]).is_ok());
        for bad in [
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
    fn gas_multiplier_rejects_nonfinite_and_subunit_values() {
        for valid in ["1.0", "1.25"] {
            assert_eq!(
                signed(&[&format!("--gas-multiplier={valid}")]).unwrap().gas_multiplier,
                valid.parse::<f64>().unwrap()
            );
        }
        for bad in ["NaN", "inf", "-inf", "0.99", "-1"] {
            assert!(
                signed(&[&format!("--gas-multiplier={bad}")]).is_err(),
                "accepted invalid multiplier {bad}"
            );
        }
        assert!(signed(&["--gas-limit=21000", "--gas-multiplier=1.0"]).is_err());
        assert!(signed(&["--gas-limit=21000"]).is_ok());
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
    fn fee_modes_enforce_pairs_conflicts_bounds_and_caps() {
        assert_eq!(
            signed(&["--gas-price=340282366920938463463374607431768211455"]).unwrap().fees,
            Fees::Legacy { gas_price: u128::MAX }
        );
        assert_eq!(
            signed(&["--max-fee-per-gas=2", "--max-priority-fee-per-gas=1"]).unwrap().fees,
            Fees::Eip1559 { max_fee_per_gas: 2, max_priority_fee_per_gas: 1 }
        );
        for flags in [
            vec!["--gas-price=1", "--max-fee-per-gas=2", "--max-priority-fee-per-gas=1"],
            vec!["--max-fee-per-gas=2"],
            vec!["--max-priority-fee-per-gas=1"],
            vec!["--gas-price=2", "--max-gas-price=1"],
            vec!["--max-fee-per-gas=2", "--max-priority-fee-per-gas=1", "--max-gas-price=1"],
            vec!["--max-fee-per-gas=1", "--max-priority-fee-per-gas=2"],
        ] {
            assert!(signed(&flags).is_err(), "accepted invalid fees {flags:?}");
        }
        for name in ["gas-price", "max-fee-per-gas", "max-priority-fee-per-gas", "max-gas-price"] {
            let error = args(&[
                &format!("--private-key={KEY}"),
                &format!("--{name}=340282366920938463463374607431768211456"),
            ])
            .unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::ValueValidation,
                "overflow was not rejected by the fee parser for {name}"
            );
        }
        assert!(signed(&["--gas-price=2", "--max-gas-price=2"]).is_ok());
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
        for flag in
            ["--private-key=private-secret-not-hex", "--mnemonic=private secret not a mnemonic"]
        {
            let value = flag.split_once('=').unwrap().1;
            let error = args(&[flag]).unwrap_err().to_string();
            assert!(!error.contains(value), "parse error exposed signing secret");
        }
        for name in ["--private-key", "--mnemonic"] {
            let error = args(&[name, "--secret-leading-hyphens"]).unwrap_err().to_string();
            assert!(
                !error.contains("--secret-leading-hyphens"),
                "parse error exposed a hyphenated secret"
            );
        }
        let conflict = args(&[&format!("--private-key={KEY}"), &format!("--mnemonic={PHRASE}")])
            .unwrap_err()
            .to_string();
        assert!(!conflict.contains(KEY) && !conflict.contains(PHRASE));
    }
}
