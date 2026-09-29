//! CLI arguments enabling the payer on a block-building node.

use std::{fmt, path::PathBuf};

use alloy_primitives::B256;
use alloy_signer::k256::ecdsa;
use alloy_signer_local::PrivateKeySigner;
use base_execution_txpool::{DEFAULT_MAX_VALIDITY_EXPIRY_SECS, DEFAULT_MAX_VALIDITY_PREDICATES};

use crate::{PayerConfig, PayerConfigError};

/// Error loading the payer from [`PayerArgs`].
#[derive(Debug, thiserror::Error)]
pub enum PayerArgsError {
    /// The payer configuration is unreadable or invalid.
    #[error(transparent)]
    Config(#[from] PayerConfigError),
    /// The key file could not be read.
    #[error("failed to read payer key file: {0}")]
    KeyFile(#[source] std::io::Error),
    /// `--payer.config` is set without a key.
    #[error("--payer.config requires --payer.key or --payer.key.path")]
    MissingKey,
    /// The key file does not hold a hex-encoded 32-byte key.
    #[error("payer key file does not hold a hex-encoded 32-byte key")]
    KeyFileFormat,
    /// The key is not a valid secp256k1 private key.
    #[error("payer key is invalid: {0}")]
    InvalidKey(#[from] ecdsa::Error),
}

/// Arguments enabling the ERC-8168 token payer.
///
/// The payer co-signs with a local key, since it signs the raw EIP-8130 payer
/// hash rather than a transaction or block payload.
#[derive(Clone, Default, PartialEq, Eq, clap::Args)]
#[command(
    next_help_heading = "Payer",
    group = clap::ArgGroup::new("payer_key").args(["payer_key_hex", "payer_key_path"]).multiple(false)
)]
pub struct PayerArgs {
    /// TOML configuration of the payer's terms and accepted tokens. Serves
    /// `payer_*` and co-signs sponsored EIP-8130 transactions into the local pool.
    #[arg(
        id = "payer_config",
        long = "payer.config",
        env = "BASE_PAYER_CONFIG",
        value_name = "PATH",
        requires = "payer_key"
    )]
    pub config: Option<PathBuf>,

    /// Private key of the payer account.
    #[arg(
        id = "payer_key_hex",
        long = "payer.key",
        env = "BASE_PAYER_KEY",
        hide_env_values = true,
        requires = "payer_config"
    )]
    pub key: Option<B256>,

    /// Path to a file holding the payer account's hex-encoded private key.
    #[arg(
        id = "payer_key_path",
        long = "payer.key.path",
        env = "BASE_PAYER_KEY_PATH",
        value_name = "PATH",
        requires = "payer_config"
    )]
    pub key_path: Option<PathBuf>,
}

impl fmt::Debug for PayerArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PayerArgs")
            .field("config", &self.config)
            .field("key", &self.key.map(|_| "<redacted>"))
            .field("key_path", &self.key_path)
            .finish()
    }
}

impl PayerArgs {
    /// Loads the payer, or returns `None` when `--payer.config` is unset.
    pub fn load(&self) -> Result<Option<PayerSponsor>, PayerArgsError> {
        let Some(path) = &self.config else {
            return Ok(None);
        };
        let config = PayerConfig::load(path)?;
        let key = match (&self.key, &self.key_path) {
            (Some(key), _) => *key,
            (None, Some(path)) => std::fs::read_to_string(path)
                .map_err(PayerArgsError::KeyFile)?
                .trim()
                .parse()
                .map_err(|_| PayerArgsError::KeyFileFormat)?,
            (None, None) => return Err(PayerArgsError::MissingKey),
        };
        Ok(Some(PayerSponsor {
            config,
            signer: PrivateKeySigner::from_bytes(&key)?,
            max_validity_predicates: DEFAULT_MAX_VALIDITY_PREDICATES,
            max_validity_expiry_secs: DEFAULT_MAX_VALIDITY_EXPIRY_SECS,
            experimental_override: false,
        }))
    }
}

/// A payer loaded from [`PayerArgs`], with the validity limits of the ingress
/// it admits co-signed transactions through.
pub struct PayerSponsor {
    /// Terms and accepted tokens.
    pub config: PayerConfig,
    /// Key of the payer account.
    pub signer: PrivateKeySigner,
    /// Maximum validity predicates the ingress accepts per transaction.
    pub max_validity_predicates: usize,
    /// Maximum validity-transaction lifetime the ingress accepts, in seconds.
    pub max_validity_expiry_secs: u64,
    /// Accept validity transactions before Cobalt activates.
    pub experimental_override: bool,
}

impl fmt::Debug for PayerSponsor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PayerSponsor")
            .field("payer", &self.config.terms.payer)
            .field("max_validity_predicates", &self.max_validity_predicates)
            .field("max_validity_expiry_secs", &self.max_validity_expiry_secs)
            .field("experimental_override", &self.experimental_override)
            .finish_non_exhaustive()
    }
}

impl PayerSponsor {
    /// Matches the ingress limits to the node's validity-transaction ingress.
    pub const fn with_validity_limits(
        mut self,
        max_validity_predicates: usize,
        experimental_override: bool,
    ) -> Self {
        self.max_validity_predicates = max_validity_predicates;
        self.experimental_override = experimental_override;
        self
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use clap::Parser;
    use tempfile::NamedTempFile;

    use super::*;

    /// Well-known development key for `0xf39F…2266`.
    const KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    const CONFIG: &str = r#"
        tokens = []

        [terms]
        payer = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
        max_expiry_secs = 10
        quote_ttl_secs = 15
        default_gas_limit = 100000

        [eth_usd]
        proxy = "0x71041dddad3595F9CEd3DcCFBe3D1F4b0a16Bb70"
        deviation_bps = 15
    "#;

    #[derive(Debug, Parser)]
    struct Cli {
        #[command(flatten)]
        payer: PayerArgs,
    }

    fn file(contents: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file
    }

    fn parse(args: &[&str]) -> Result<PayerArgs, clap::Error> {
        Cli::try_parse_from(std::iter::once("node").chain(args.iter().copied()))
            .map(|cli| cli.payer)
    }

    #[test]
    fn is_disabled_without_a_config() {
        assert!(parse(&[]).unwrap().load().unwrap().is_none());
    }

    #[test]
    fn loads_config_with_a_key_file() {
        let config = file(CONFIG);
        let key = file(&format!("{KEY}\n"));
        let args = parse(&[
            "--payer.config",
            config.path().to_str().unwrap(),
            "--payer.key.path",
            key.path().to_str().unwrap(),
        ])
        .unwrap();

        let sponsor = args.load().unwrap().unwrap();

        assert_eq!(sponsor.signer.address(), sponsor.config.terms.payer);
        assert_eq!(sponsor.max_validity_predicates, DEFAULT_MAX_VALIDITY_PREDICATES);
    }

    #[test]
    fn requires_exactly_one_key_source_with_a_config() {
        let config = file(CONFIG);
        let key = file(KEY);
        let config_path = config.path().to_str().unwrap();
        let key_path = key.path().to_str().unwrap();

        assert!(parse(&["--payer.config", config_path]).is_err());
        assert!(parse(&["--payer.key", KEY]).is_err());
        assert!(
            parse(&[
                "--payer.config",
                config_path,
                "--payer.key",
                KEY,
                "--payer.key.path",
                key_path
            ])
            .is_err()
        );
    }

    #[test]
    fn rejects_malformed_key_file() {
        let config = file(CONFIG);
        let key = file("not a key");
        let args = parse(&[
            "--payer.config",
            config.path().to_str().unwrap(),
            "--payer.key.path",
            key.path().to_str().unwrap(),
        ])
        .unwrap();

        assert!(matches!(args.load(), Err(PayerArgsError::KeyFileFormat)));
    }

    #[test]
    fn debug_output_redacts_the_key() {
        let config = file(CONFIG);
        let args = parse(&["--payer.config", config.path().to_str().unwrap(), "--payer.key", KEY])
            .unwrap();

        assert!(!format!("{args:?}").contains(&KEY[2..]));
    }
}
