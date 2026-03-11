use std::time::Duration;

use alloy_primitives::Address;
use alloy_signer::k256::ecdsa::SigningKey;
use alloy_signer_local::PrivateKeySigner;
use clap::Parser;
use url::Url;

use crate::{RegistrarError, Result};

/// Signing configuration for L1 transaction submission.
#[derive(Clone)]
pub enum SigningConfig {
    /// HTTP signer sidecar (production).
    Remote {
        /// Signer sidecar JSON-RPC endpoint URL.
        endpoint: Url,
        /// Manager address for signing registration transactions.
        address: Address,
    },
    /// Direct in-process private key. **Development / testing only.**
    Local {
        /// Private key signer.
        signer: PrivateKeySigner,
    },
}

impl std::fmt::Debug for SigningConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Remote { endpoint, address } => f
                .debug_struct("Remote")
                .field("endpoint", endpoint)
                .field("address", address)
                .finish(),
            Self::Local { signer } => {
                f.debug_struct("Local").field("address", &signer.address()).finish()
            }
        }
    }
}

/// All configuration for the prover registrar, parsed from CLI args and env vars.
///
/// After [`clap::Parser::parse`], call [`RegistrarConfig::validate`] to check
/// for logical conflicts. Use [`RegistrarConfig::signing_config`] to obtain the
/// resolved [`SigningConfig`].
#[derive(Debug, Clone, Parser)]
#[command(name = "prover-registrar", version, about)]
pub struct RegistrarConfig {
    // ── L1 ────────────────────────────────────────────────────────────────────
    /// L1 Ethereum RPC endpoint.
    #[arg(long, env = "REGISTRAR_L1_RPC_URL")]
    pub l1_rpc_url: Url,

    /// `SystemConfigGlobal` contract address on L1.
    #[arg(long, env = "REGISTRAR_SYSTEM_CONFIG_GLOBAL_ADDRESS")]
    pub system_config_global_address: Address,

    // ── AWS ───────────────────────────────────────────────────────────────────
    /// AWS ALB target group ARN for prover instance discovery.
    #[arg(long, env = "REGISTRAR_TARGET_GROUP_ARN")]
    pub target_group_arn: String,

    /// AWS region.
    #[arg(long, env = "REGISTRAR_AWS_REGION", default_value = "us-east-1")]
    pub aws_region: String,

    /// JSON-RPC port to poll on each prover instance.
    #[arg(long, env = "REGISTRAR_PROVER_PORT", default_value_t = 8000)]
    pub prover_port: u16,

    // ── Signing ───────────────────────────────────────────────────────────────
    /// HTTP signer sidecar URL (production). Mutually exclusive with `--private-key`.
    #[arg(long, env = "REGISTRAR_SIGNER_ENDPOINT", conflicts_with = "private_key")]
    pub signer_endpoint: Option<Url>,

    /// Manager address for signing txs (required with `--signer-endpoint`).
    #[arg(long, env = "REGISTRAR_SIGNER_ADDRESS")]
    pub signer_address: Option<Address>,

    /// Hex-encoded private key. **Local development only** — use signer sidecar in production.
    #[arg(long, env = "REGISTRAR_PRIVATE_KEY", conflicts_with = "signer_endpoint")]
    pub private_key: Option<String>,

    // ── Boundless ─────────────────────────────────────────────────────────────
    /// Boundless Network RPC URL.
    #[arg(long, env = "REGISTRAR_BOUNDLESS_RPC_URL")]
    pub boundless_rpc_url: Url,

    /// Hex-encoded private key for Boundless Network proving fees.
    #[arg(long, env = "REGISTRAR_BOUNDLESS_PRIVATE_KEY")]
    pub boundless_private_key: String,

    /// IPFS URL of the Nitro attestation verifier ELF uploaded via `nitro-attest-cli`.
    #[arg(long, env = "REGISTRAR_BOUNDLESS_VERIFIER_PROGRAM_URL")]
    pub boundless_verifier_program_url: Url,

    /// Minimum price in wei per cycle for Boundless proof requests.
    #[arg(long, env = "REGISTRAR_BOUNDLESS_MIN_PRICE", default_value_t = 100_000)]
    pub boundless_min_price: u64,

    /// Maximum price in wei per cycle for Boundless proof requests.
    #[arg(long, env = "REGISTRAR_BOUNDLESS_MAX_PRICE", default_value_t = 1_000_000)]
    pub boundless_max_price: u64,

    /// Proof generation timeout in seconds.
    #[arg(long, env = "REGISTRAR_BOUNDLESS_TIMEOUT_SECS", default_value_t = 600)]
    pub boundless_timeout_secs: u64,

    /// `NitroEnclaveVerifier` contract address for certificate caching (optional).
    #[arg(long, env = "REGISTRAR_NITRO_VERIFIER_ADDRESS")]
    pub nitro_verifier_address: Option<Address>,

    // ── Polling / Server ──────────────────────────────────────────────────────
    /// Interval between discovery and registration poll cycles, in seconds.
    #[arg(long, env = "REGISTRAR_POLL_INTERVAL_SECS", default_value_t = 30)]
    pub poll_interval_secs: u64,

    /// Port for the health check and Prometheus metrics HTTP server.
    #[arg(long, env = "REGISTRAR_HEALTH_PORT", default_value_t = 7300)]
    pub health_port: u16,
}

impl RegistrarConfig {
    /// Validate the configuration for logical conflicts and missing required fields.
    ///
    /// Returns an error if the signing configuration is ambiguous, price bounds are
    /// inverted, or any required-when-present field is absent.
    pub fn validate(&self) -> Result<()> {
        match (&self.private_key, &self.signer_endpoint, &self.signer_address) {
            // Remote sidecar requires both endpoint and address.
            (None, Some(_), None) | (None, None, _) => {
                return Err(RegistrarError::Config(
                    "provide either --private-key or both --signer-endpoint and \
                     --signer-address"
                        .into(),
                ));
            }
            _ => {}
        }

        if self.boundless_min_price > self.boundless_max_price {
            return Err(RegistrarError::Config(
                "--boundless-min-price must not exceed --boundless-max-price".into(),
            ));
        }

        if self.poll_interval_secs == 0 {
            return Err(RegistrarError::Config(
                "--poll-interval-secs must be greater than 0".into(),
            ));
        }

        Ok(())
    }

    /// Build the [`SigningConfig`] from the parsed signing fields.
    ///
    /// Returns an error if the private key cannot be decoded or if the
    /// configuration is ambiguous. Call [`validate`][Self::validate] first.
    pub fn signing_config(&self) -> Result<SigningConfig> {
        match (&self.private_key, &self.signer_endpoint, &self.signer_address) {
            (Some(pk), None, None) => {
                let hex_str = pk.strip_prefix("0x").unwrap_or(pk);
                let key_bytes = hex::decode(hex_str)
                    .map_err(|e| RegistrarError::Config(format!("invalid private key hex: {e}")))?;
                let signing_key = SigningKey::from_slice(&key_bytes)
                    .map_err(|e| RegistrarError::Config(format!("invalid private key: {e}")))?;
                Ok(SigningConfig::Local { signer: PrivateKeySigner::from_signing_key(signing_key) })
            }
            (None, Some(endpoint), Some(address)) => {
                Ok(SigningConfig::Remote { endpoint: endpoint.clone(), address: *address })
            }
            _ => Err(RegistrarError::Config(
                "ambiguous signing config; call validate() before signing_config()".into(),
            )),
        }
    }

    /// Return the poll interval as a [`Duration`].
    pub const fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_interval_secs)
    }

    /// Return the Boundless proof generation timeout as a [`Duration`].
    pub const fn boundless_timeout(&self) -> Duration {
        Duration::from_secs(self.boundless_timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args() -> Vec<&'static str> {
        vec![
            "prover-registrar",
            "--l1-rpc-url",
            "http://localhost:8545",
            "--system-config-global-address",
            "0x0000000000000000000000000000000000000001",
            "--target-group-arn",
            "arn:aws:elasticloadbalancing:us-east-1:123456789012:targetgroup/test/abc",
            "--private-key",
            "0x0101010101010101010101010101010101010101010101010101010101010101",
            "--boundless-rpc-url",
            "http://localhost:9545",
            "--boundless-private-key",
            "0202020202020202020202020202020202020202020202020202020202020202",
            "--boundless-verifier-program-url",
            "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
        ]
    }

    #[test]
    fn valid_local_key_config_passes_validate() {
        let config = RegistrarConfig::parse_from(base_args());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn zero_poll_interval_fails_validate() {
        let mut args = base_args();
        args.extend(["--poll-interval-secs", "0"]);
        let config = RegistrarConfig::parse_from(args);
        assert!(config.validate().is_err());
    }

    #[test]
    fn inverted_price_bounds_fail_validate() {
        let mut args = base_args();
        args.extend(["--boundless-min-price", "9999", "--boundless-max-price", "1"]);
        let config = RegistrarConfig::parse_from(args);
        assert!(config.validate().is_err());
    }

    #[test]
    fn signing_config_local_parses_key() {
        let config = RegistrarConfig::parse_from(base_args());
        let signing = config.signing_config().unwrap();
        assert!(matches!(signing, SigningConfig::Local { .. }));
    }

    #[test]
    fn poll_interval_returns_duration() {
        let config = RegistrarConfig::parse_from(base_args());
        assert_eq!(config.poll_interval(), Duration::from_secs(30));
    }
}
