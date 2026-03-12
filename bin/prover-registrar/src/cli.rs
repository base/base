//! CLI argument parsing and config construction for the prover registrar.

use std::time::Duration;

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use base_proof_tee_registrar::{
    BoundlessConfig, RegistrarConfig, RegistrarError, RemoteSignerConfig, SigningConfig,
};
use clap::{ArgGroup, Args, Parser};
use url::Url;

/// Prover Registrar — automated TEE signer registration service.
#[derive(Parser)]
#[command(
    name = "prover-registrar",
    version,
    about,
    group(
        ArgGroup::new("signing_method")
            .required(true)
            .args(["private_key", "signer_endpoint"])
    )
)]
pub(crate) struct Cli {
    // ── L1 ────────────────────────────────────────────────────────────────────
    /// L1 Ethereum RPC endpoint.
    #[arg(long, env = "REGISTRAR_L1_RPC_URL")]
    l1_rpc_url: Url,

    /// `TEEProverRegistry` contract address on L1.
    #[arg(long, env = "REGISTRAR_TEE_PROVER_REGISTRY_ADDRESS")]
    tee_prover_registry_address: Address,

    // ── AWS ───────────────────────────────────────────────────────────────────
    /// AWS ALB target group ARN for prover instance discovery.
    #[arg(long, env = "REGISTRAR_TARGET_GROUP_ARN")]
    target_group_arn: String,

    /// AWS region.
    #[arg(long, env = "REGISTRAR_AWS_REGION")]
    aws_region: String,

    /// JSON-RPC port to poll on each prover instance.
    #[arg(long, env = "REGISTRAR_PROVER_PORT", default_value_t = 8000)]
    prover_port: u16,

    // ── Signing ───────────────────────────────────────────────────────────────
    /// HTTP signer sidecar URL (production). Mutually exclusive with `--private-key`.
    #[arg(
        long,
        env = "REGISTRAR_SIGNER_ENDPOINT",
        conflicts_with = "private_key",
        requires = "signer_address"
    )]
    signer_endpoint: Option<Url>,

    /// Manager address for signing txs (required with `--signer-endpoint`).
    #[arg(long, env = "REGISTRAR_SIGNER_ADDRESS", requires = "signer_endpoint")]
    signer_address: Option<Address>,

    /// Hex-encoded private key. **Local development only** — use signer sidecar in production.
    #[arg(long, env = "REGISTRAR_PRIVATE_KEY", conflicts_with = "signer_endpoint")]
    private_key: Option<String>,

    // ── Boundless ─────────────────────────────────────────────────────────────
    #[command(flatten)]
    boundless: BoundlessArgs,

    // ── Polling / Server ──────────────────────────────────────────────────────
    /// Interval between discovery and registration poll cycles, in seconds.
    #[arg(long, env = "REGISTRAR_POLL_INTERVAL_SECS", default_value_t = 30)]
    poll_interval_secs: u64,

    /// Port for the health check and Prometheus metrics HTTP server.
    #[arg(long, env = "REGISTRAR_HEALTH_PORT", default_value_t = 7300)]
    health_port: u16,
}

/// Boundless Network CLI arguments.
#[derive(Args)]
struct BoundlessArgs {
    /// Boundless Network RPC URL.
    #[arg(long, env = "REGISTRAR_BOUNDLESS_RPC_URL")]
    boundless_rpc_url: Url,

    /// Hex-encoded private key for Boundless Network proving fees.
    #[arg(long, env = "REGISTRAR_BOUNDLESS_PRIVATE_KEY")]
    boundless_private_key: String,

    /// IPFS URL of the Nitro attestation verifier ELF uploaded via `nitro-attest-cli`.
    #[arg(long, env = "REGISTRAR_BOUNDLESS_VERIFIER_PROGRAM_URL")]
    boundless_verifier_program_url: Url,

    /// Minimum price in wei per cycle for Boundless proof requests.
    #[arg(long, env = "REGISTRAR_BOUNDLESS_MIN_PRICE", default_value_t = 100_000)]
    boundless_min_price: u64,

    /// Maximum price in wei per cycle for Boundless proof requests.
    #[arg(long, env = "REGISTRAR_BOUNDLESS_MAX_PRICE", default_value_t = 1_000_000)]
    boundless_max_price: u64,

    /// Proof generation timeout in seconds.
    #[arg(long, env = "REGISTRAR_BOUNDLESS_TIMEOUT_SECS", default_value_t = 600)]
    boundless_timeout_secs: u64,

    /// `NitroEnclaveVerifier` contract address for certificate caching (optional).
    #[arg(long, env = "REGISTRAR_NITRO_VERIFIER_ADDRESS")]
    nitro_verifier_address: Option<Address>,
}

/// Parse a hex-encoded secp256k1 private key string into a [`PrivateKeySigner`].
fn parse_private_key(field: &str, s: &str) -> Result<PrivateKeySigner, RegistrarError> {
    s.strip_prefix("0x")
        .unwrap_or(s)
        .parse::<PrivateKeySigner>()
        .map_err(|e| RegistrarError::Config(format!("{field}: {e}")))
}

impl Cli {
    /// Validate the CLI arguments for logical conflicts and parse into a [`RegistrarConfig`].
    pub(crate) fn into_config(self) -> Result<RegistrarConfig, RegistrarError> {
        // Validate signing config and resolve to SigningConfig.
        let signing = match (&self.private_key, &self.signer_endpoint, &self.signer_address) {
            (Some(pk), None, None) => SigningConfig::Local(parse_private_key("--private-key", pk)?),
            (None, Some(endpoint), Some(address)) => SigningConfig::Remote(RemoteSignerConfig {
                endpoint: endpoint.clone(),
                address: *address,
            }),
            _ => {
                return Err(RegistrarError::Config(
                    "provide either --private-key or both --signer-endpoint and \
                     --signer-address"
                        .into(),
                ));
            }
        };

        if self.boundless.boundless_min_price > self.boundless.boundless_max_price {
            return Err(RegistrarError::Config(
                "--boundless-min-price must not exceed --boundless-max-price".into(),
            ));
        }

        if self.boundless.boundless_timeout_secs == 0 {
            return Err(RegistrarError::Config(
                "--boundless-timeout-secs must be greater than 0".into(),
            ));
        }

        if self.poll_interval_secs == 0 {
            return Err(RegistrarError::Config(
                "--poll-interval-secs must be greater than 0".into(),
            ));
        }

        Ok(RegistrarConfig {
            l1_rpc_url: self.l1_rpc_url,
            tee_prover_registry_address: self.tee_prover_registry_address,
            target_group_arn: self.target_group_arn,
            aws_region: self.aws_region,
            prover_port: self.prover_port,
            signing,
            boundless: BoundlessConfig {
                rpc_url: self.boundless.boundless_rpc_url,
                signer: parse_private_key(
                    "--boundless-private-key",
                    &self.boundless.boundless_private_key,
                )?,
                verifier_program_url: self.boundless.boundless_verifier_program_url,
                min_price: self.boundless.boundless_min_price,
                max_price: self.boundless.boundless_max_price,
                timeout: Duration::from_secs(self.boundless.boundless_timeout_secs),
                nitro_verifier_address: self.boundless.nitro_verifier_address,
            },
            poll_interval: Duration::from_secs(self.poll_interval_secs),
            health_port: self.health_port,
        })
    }

    /// Run the registrar service.
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let _config = self.into_config()?;
        // TODO(CHAIN-3455): start RegistrationDriver
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn base_args() -> Vec<&'static str> {
        vec![
            "prover-registrar",
            "--l1-rpc-url",
            "http://localhost:8545",
            "--tee-prover-registry-address",
            "0x0000000000000000000000000000000000000001",
            "--target-group-arn",
            "arn:aws:elasticloadbalancing:us-east-1:123456789012:targetgroup/test/abc",
            "--aws-region",
            "us-east-1",
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

    fn remote_args() -> Vec<&'static str> {
        vec![
            "prover-registrar",
            "--l1-rpc-url",
            "http://localhost:8545",
            "--tee-prover-registry-address",
            "0x0000000000000000000000000000000000000001",
            "--target-group-arn",
            "arn:aws:elasticloadbalancing:us-east-1:123456789012:targetgroup/test/abc",
            "--aws-region",
            "us-east-1",
            "--signer-endpoint",
            "http://localhost:8546",
            "--signer-address",
            "0x0000000000000000000000000000000000000002",
            "--boundless-rpc-url",
            "http://localhost:9545",
            "--boundless-private-key",
            "0202020202020202020202020202020202020202020202020202020202020202",
            "--boundless-verifier-program-url",
            "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
        ]
    }

    #[test]
    fn valid_local_key_config_into_config() {
        assert!(Cli::parse_from(base_args()).into_config().is_ok());
    }

    #[test]
    fn valid_remote_signer_config_into_config() {
        assert!(Cli::parse_from(remote_args()).into_config().is_ok());
    }

    #[test]
    fn into_config_local_returns_local_signing() {
        let config = Cli::parse_from(base_args()).into_config().unwrap();
        assert!(matches!(config.signing, SigningConfig::Local(_)));
    }

    #[test]
    fn into_config_remote_returns_remote_signing() {
        let config = Cli::parse_from(remote_args()).into_config().unwrap();
        assert!(matches!(config.signing, SigningConfig::Remote(_)));
    }

    #[test]
    fn no_signing_method_fails_clap_parse() {
        let args = vec![
            "prover-registrar",
            "--l1-rpc-url",
            "http://localhost:8545",
            "--tee-prover-registry-address",
            "0x0000000000000000000000000000000000000001",
            "--target-group-arn",
            "arn:aws:elasticloadbalancing:us-east-1:123456789012:targetgroup/test/abc",
            "--aws-region",
            "us-east-1",
            "--boundless-rpc-url",
            "http://localhost:9545",
            "--boundless-private-key",
            "0202020202020202020202020202020202020202020202020202020202020202",
            "--boundless-verifier-program-url",
            "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
        ];
        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn signer_endpoint_without_address_fails_into_config() {
        let args = vec![
            "prover-registrar",
            "--l1-rpc-url",
            "http://localhost:8545",
            "--tee-prover-registry-address",
            "0x0000000000000000000000000000000000000001",
            "--target-group-arn",
            "arn:aws:elasticloadbalancing:us-east-1:123456789012:targetgroup/test/abc",
            "--aws-region",
            "us-east-1",
            "--signer-endpoint",
            "http://localhost:8546",
            "--boundless-rpc-url",
            "http://localhost:9545",
            "--boundless-private-key",
            "0202020202020202020202020202020202020202020202020202020202020202",
            "--boundless-verifier-program-url",
            "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi",
        ];
        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn zero_poll_interval_fails_into_config() {
        let mut args = base_args();
        args.extend(["--poll-interval-secs", "0"]);
        assert!(
            Cli::try_parse_from(args).expect("clap should parse these args").into_config().is_err()
        );
    }

    #[test]
    fn zero_boundless_timeout_fails_into_config() {
        let mut args = base_args();
        args.extend(["--boundless-timeout-secs", "0"]);
        assert!(
            Cli::try_parse_from(args).expect("clap should parse these args").into_config().is_err()
        );
    }

    #[test]
    fn inverted_price_bounds_fail_into_config() {
        let mut args = base_args();
        args.extend(["--boundless-min-price", "9999", "--boundless-max-price", "1"]);
        assert!(
            Cli::try_parse_from(args).expect("clap should parse these args").into_config().is_err()
        );
    }

    #[test]
    fn poll_interval_returns_duration() {
        let config = Cli::parse_from(base_args()).into_config().unwrap();
        assert_eq!(config.poll_interval, Duration::from_secs(30));
    }
}
