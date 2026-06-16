//! CLI argument parsing and config construction for the prover registrar.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_primitives::{Address, hex};
use alloy_provider::ProviderBuilder;
use base_balance_monitor::BalanceMonitorLayer;
use base_cli_utils::RuntimeManager;
use base_health::HealthServer;
use base_proof_tee_nitro_attestation_prover::BoundlessProver;
use base_proof_tee_registrar::{
    AwsTargetGroupDiscovery, CertManager, DEFAULT_CRL_FETCH_TIMEOUT_SECS, DEFAULT_MAX_CONCURRENCY,
    DEFAULT_MAX_TX_RETRIES, DEFAULT_TX_RETRY_DELAY_SECS,
    DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS, DriverConfig, NitroVerifierContractClient,
    ProverClient, RegistrarError, RegistrarMetrics, RegistrationDriver, RegistryContractClient,
    SignerManager, SignerManagerConfig,
};
use base_tx_manager::{BaseTxMetrics, SignerConfig, SimpleTxManager, TxManagerConfig};
use boundless_market::{
    alloy::signers::local::PrivateKeySigner,
    price_oracle::{Amount, Asset},
};
use clap::{Args, Parser};
use eyre::WrapErr;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

// Generate env-var helper and CLI structs with the `BASE_REGISTRAR_` prefix.
base_cli_utils::define_cli_env!("BASE_REGISTRAR");
base_cli_utils::define_log_args!("BASE_REGISTRAR");
base_cli_utils::define_metrics_args!("BASE_REGISTRAR", 7300);
base_cli_utils::define_health_args!("BASE_REGISTRAR", 8080);
base_tx_manager::define_signer_cli!("BASE_REGISTRAR");
base_tx_manager::define_tx_manager_cli!("BASE_REGISTRAR");

/// Default trusted certificate prefix length (root cert only).
const DEFAULT_TRUSTED_CERTS_PREFIX: u8 = 1;
const DEFAULT_MAX_RECOVERY_ATTEMPTS: u32 = 5;
const DEFAULT_MAX_ATTESTATION_AGE_SECS: u64 = 3300;

/// Prover Registrar — automated TEE signer registration service.
#[derive(Parser)]
#[command(name = "prover-registrar", version, about)]
pub(crate) struct Cli {
    /// L1 Ethereum RPC endpoint.
    #[arg(long, env = cli_env!("L1_RPC_URL"))]
    l1_rpc_url: Url,

    /// `TEEProverRegistry` contract address on L1.
    #[arg(long, env = cli_env!("TEE_PROVER_REGISTRY_ADDRESS"))]
    tee_prover_registry_address: Address,

    /// L1 chain ID (used to validate the RPC connection).
    #[arg(long, env = cli_env!("L1_CHAIN_ID"))]
    l1_chain_id: u64,
    /// AWS ALB target group ARN for prover instance discovery.
    #[arg(long, env = cli_env!("TARGET_GROUP_ARN"))]
    target_group_arn: String,

    /// AWS region (e.g. `us-east-1`).
    #[arg(long, env = cli_env!("AWS_REGION"))]
    aws_region: String,

    /// JSON-RPC port to poll on each prover instance.
    #[arg(long, env = cli_env!("PROVER_PORT"), default_value_t = 8000)]
    prover_port: u16,
    /// Signer configuration (local private key or remote sidecar).
    #[command(flatten)]
    signer: SignerCli,
    /// Transaction manager configuration (fee limits, confirmations, timeouts).
    #[command(flatten)]
    tx_manager: TxManagerCli,

    /// Hex-encoded guest program image ID.
    #[arg(long, env = cli_env!("IMAGE_ID"), value_parser = parse_image_id)]
    image_id: [u32; 8],
    #[command(flatten)]
    boundless: BoundlessArgs,
    /// Interval between discovery and registration poll cycles, in seconds.
    #[arg(
        long,
        env = cli_env!("POLL_INTERVAL_SECS"),
        default_value_t = 30,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    poll_interval_secs: u64,

    /// Timeout for JSON-RPC calls to prover instances, in seconds.
    #[arg(
        long,
        env = cli_env!("PROVER_TIMEOUT_SECS"),
        default_value_t = 30,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    prover_timeout_secs: u64,

    /// Maximum number of instances to process concurrently within a single
    /// registration cycle. Each instance may trigger a ~20-minute proof
    /// generation, so this limits concurrent proof work.
    #[arg(
        long,
        env = cli_env!("MAX_CONCURRENCY"),
        default_value_t = DEFAULT_MAX_CONCURRENCY,
        value_parser = parse_nonzero_usize
    )]
    max_concurrency: usize,
    /// Maximum number of transaction submission retries for transient errors.
    #[arg(long, env = cli_env!("MAX_TX_RETRIES"), default_value_t = DEFAULT_MAX_TX_RETRIES)]
    max_tx_retries: u32,

    /// Delay between transaction submission retries, in seconds.
    #[arg(
        long,
        env = cli_env!("TX_RETRY_DELAY_SECS"),
        default_value_t = DEFAULT_TX_RETRY_DELAY_SECS,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    tx_retry_delay_secs: u64,
    /// Duration (seconds) after EC2 launch during which unhealthy instances
    /// are still eligible for registration. New instances may fail ALB health
    /// checks while the application initializes. Set to 0 to disable.
    #[arg(long, env = cli_env!("UNHEALTHY_REGISTRATION_WINDOW_SECS"), default_value_t = DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS)]
    unhealthy_registration_window_secs: u64,
    /// Enable on-demand CRL checking at registration time.
    /// When enabled, intermediate certificates are checked against CRL
    /// distribution points before signer registration. Revoked certificates
    /// trigger a `revokeCert` transaction onchain.
    #[arg(
        long,
        env = cli_env!("CRL_CHECK_ENABLED"),
        requires = "crl_nitro_verifier_address"
    )]
    crl_check_enabled: bool,

    /// `NitroEnclaveVerifier` contract address. Required when
    /// `--crl-check-enabled` is set; consulted both for the durable onchain
    /// `revokedCerts` pre-check and as the destination for outgoing
    /// `revokeCert` transactions.
    ///
    /// The `crl-` prefix is retained for backward compatibility with
    /// existing production deployments (introduced in #1984).
    #[arg(long, env = cli_env!("CRL_NITRO_VERIFIER_ADDRESS"))]
    crl_nitro_verifier_address: Option<Address>,
    #[command(flatten)]
    health: HealthArgs,
    #[command(flatten)]
    log: LogArgs,
    #[command(flatten)]
    metrics: MetricsArgs,
}

/// Boundless Network CLI arguments.
#[derive(Args)]
struct BoundlessArgs {
    /// Boundless Network RPC URL.
    #[arg(long, env = cli_env!("BOUNDLESS_RPC_URL"))]
    boundless_rpc_url: Url,

    /// Hex-encoded private key for Boundless Network proving fees.
    #[arg(long, env = cli_env!("BOUNDLESS_PRIVATE_KEY"), value_parser = parse_boundless_private_key)]
    boundless_private_key: PrivateKeySigner,

    /// HTTP(S) URL of the Nitro attestation verifier ELF (e.g. Pinata IPFS gateway URL).
    #[arg(long, env = cli_env!("BOUNDLESS_VERIFIER_PROGRAM_URL"))]
    boundless_verifier_program_url: Url,

    /// Interval between Boundless fulfillment status checks, in seconds.
    #[arg(long, env = cli_env!("BOUNDLESS_POLL_INTERVAL_SECS"), default_value_t = 5)]
    boundless_poll_interval_secs: u64,

    /// Client-side fulfillment poll budget, in seconds.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_TIMEOUT_SECS"),
        default_value_t = 1260,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    boundless_timeout_secs: u64,

    /// Minimum Boundless offer price in ETH for each submitted proof request.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_MIN_PRICE_ETH"),
        requires = "boundless_max_price_eth",
        value_parser = parse_boundless_eth_amount
    )]
    boundless_min_price_eth: Option<Amount>,

    /// Maximum Boundless offer price in ETH for each submitted proof request.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_MAX_PRICE_ETH"),
        requires = "boundless_min_price_eth",
        value_parser = parse_boundless_eth_amount
    )]
    boundless_max_price_eth: Option<Amount>,

    /// Optional duration in seconds for the Boundless offer price ramp.
    #[arg(long, env = cli_env!("BOUNDLESS_OFFER_RAMP_UP_PERIOD_SECS"))]
    boundless_offer_ramp_up_period_secs: Option<u32>,

    /// Maximum lock and delivery window for a Boundless proof request.
    #[arg(long, env = cli_env!("BOUNDLESS_OFFER_LOCK_TIMEOUT_SECS"))]
    boundless_offer_lock_timeout_secs: Option<u32>,

    /// Delay before Boundless bidding starts.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_OFFER_BIDDING_START_DELAY_SECS"),
        default_value_t = 0
    )]
    boundless_offer_bidding_start_delay_secs: u64,

    /// Maximum number of deterministic request-ID slots to probe when
    /// recovering in-flight proofs after an instance rotation.
    #[arg(
        long,
        env = cli_env!("BOUNDLESS_MAX_RECOVERY_ATTEMPTS"),
        default_value_t = DEFAULT_MAX_RECOVERY_ATTEMPTS
    )]
    boundless_max_recovery_attempts: u32,

    /// Maximum age (in seconds) of a recovered proof's attestation timestamp
    /// before it is considered stale. Should be slightly below the onchain
    /// `MAX_AGE` to account for clock skew. Defaults to 3300 s (55 minutes).
    #[arg(
        long,
        env = cli_env!("MAX_ATTESTATION_AGE_SECS"),
        default_value_t = DEFAULT_MAX_ATTESTATION_AGE_SECS
    )]
    max_attestation_age_secs: u64,
}

/// Parse a hex-encoded Boundless private key string into a [`PrivateKeySigner`].
fn parse_boundless_private_key(s: &str) -> std::result::Result<PrivateKeySigner, String> {
    s.strip_prefix("0x")
        .unwrap_or(s)
        .parse::<PrivateKeySigner>()
        .map_err(|e| format!("--boundless-private-key: {e}"))
}

/// Parse a hex-encoded image ID string into `[u32; 8]`.
fn parse_image_id(s: &str) -> std::result::Result<[u32; 8], String> {
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(s.strip_prefix("0x").unwrap_or(s), &mut bytes)
        .map_err(|e| format!("--image-id: {e}"))?;

    let mut id = [0u32; 8];
    for (i, chunk) in bytes.chunks_exact(4).enumerate() {
        id[i] = u32::from_le_bytes(chunk.try_into().unwrap());
    }
    Ok(id)
}

fn parse_nonzero_usize(s: &str) -> std::result::Result<usize, String> {
    let value = s.parse::<usize>().map_err(|e| e.to_string())?;
    (value > 0).then_some(value).ok_or_else(|| "value must be at least 1".into())
}

/// Parse an ETH-denominated Boundless offer price.
fn parse_boundless_eth_amount(s: &str) -> std::result::Result<Amount, String> {
    Amount::parse_with_allowed(s, &[Asset::ETH], Some(Asset::ETH))
        .map_err(|e| format!("Boundless ETH amount: {e}"))
}

impl Cli {
    fn boundless_prover(&self) -> std::result::Result<BoundlessProver, RegistrarError> {
        let offer_min_price = self.boundless.boundless_min_price_eth.clone();
        let offer_max_price = self.boundless.boundless_max_price_eth.clone();

        if let (Some(min_price), Some(max_price)) = (&offer_min_price, &offer_max_price) {
            if max_price.value < min_price.value {
                return Err(RegistrarError::Config(
                    "--boundless-max-price-eth must be greater than or equal to --boundless-min-price-eth"
                        .into(),
                ));
            }
        }

        let mut prover = BoundlessProver::new(
            self.boundless.boundless_rpc_url.clone(),
            self.boundless.boundless_private_key.clone(),
            self.boundless.boundless_verifier_program_url.clone(),
            self.image_id,
            Duration::from_secs(self.boundless.boundless_poll_interval_secs),
            Duration::from_secs(self.boundless.boundless_timeout_secs),
            DEFAULT_TRUSTED_CERTS_PREFIX,
            self.boundless.boundless_max_recovery_attempts,
            Duration::from_secs(self.boundless.max_attestation_age_secs),
        );
        prover.offer_min_price = offer_min_price;
        prover.offer_max_price = offer_max_price;
        prover.offer_ramp_up_period_secs = self.boundless.boundless_offer_ramp_up_period_secs;
        prover.offer_lock_timeout_secs = self.boundless.boundless_offer_lock_timeout_secs;
        prover.offer_bidding_start_delay_secs =
            self.boundless.boundless_offer_bidding_start_delay_secs;
        Ok(prover)
    }

    /// Run the registrar service.
    pub(crate) async fn run(mut self) -> eyre::Result<()> {
        // Extract observability args before config parsing consumes self.
        // LogArgs/MetricsArgs are binary-layer concerns.
        let log_config: base_cli_utils::LogConfig = std::mem::take(&mut self.log).into();
        let metrics_config: base_cli_utils::MetricsConfig =
            std::mem::take(&mut self.metrics).into();

        let signing = SignerConfig::try_from(self.signer.clone())
            .map_err(|e| RegistrarError::Config(format!("signer: {e}")))?;
        let tx_manager_config = TxManagerConfig::try_from(self.tx_manager.clone())
            .map_err(|e| RegistrarError::Config(format!("tx-manager: {e}")))?;
        let boundless_prover = self.boundless_prover()?;

        log_config.init_tracing_subscriber()?;

        // Install the default rustls CryptoProvider before any TLS connections are created.
        let _ = rustls::crypto::ring::default_provider().install_default();

        info!(version = env!("CARGO_PKG_VERSION"), "Registrar starting");

        let cancel = CancellationToken::new();
        let signal_handle = RuntimeManager::install_signal_handler(cancel.clone());

        let metrics_enabled = metrics_config.enabled;
        metrics_config
            .init_with(|| {
                base_cli_utils::register_version_metrics!();
                RegistrarMetrics::up().set(1.0);
            })
            .wrap_err("failed to install Prometheus recorder")?;

        let l1_addr = signing.address();
        let provider = if metrics_enabled {
            let (layer, balance_rx) = BalanceMonitorLayer::new(
                l1_addr,
                cancel.clone(),
                BalanceMonitorLayer::DEFAULT_POLL_INTERVAL,
            );
            let provider =
                ProviderBuilder::new().layer(layer).connect_http(self.l1_rpc_url.clone());
            tokio::spawn(async move {
                let mut rx = balance_rx;
                while rx.changed().await.is_ok() {
                    RegistrarMetrics::account_balance_wei().set(f64::from(*rx.borrow_and_update()));
                }
            });
            info!(%l1_addr, "L1 balance monitor started");

            let bl_addr = boundless_prover.signer.address();
            let (bl_layer, bl_balance_rx) = BalanceMonitorLayer::new(
                bl_addr,
                cancel.clone(),
                BalanceMonitorLayer::DEFAULT_POLL_INTERVAL,
            );
            let _bl_provider = ProviderBuilder::new()
                .layer(bl_layer)
                .connect_http(boundless_prover.rpc_url.clone());
            tokio::spawn(async move {
                let mut rx = bl_balance_rx;
                while rx.changed().await.is_ok() {
                    RegistrarMetrics::boundless_balance_wei()
                        .set(f64::from(*rx.borrow_and_update()));
                }
            });
            info!(%bl_addr, "Boundless balance monitor started");

            provider
        } else {
            ProviderBuilder::new().connect_http(self.l1_rpc_url.clone())
        };

        let tx_manager = SimpleTxManager::new(
            provider,
            signing,
            tx_manager_config,
            self.l1_chain_id,
            Arc::new(BaseTxMetrics::new("registrar")),
        )
        .await?;

        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(self.aws_region.clone()))
            .load()
            .await;
        let elb_client = aws_sdk_elasticloadbalancingv2::Client::new(&aws_config);
        let ec2_client = aws_sdk_ec2::Client::new(&aws_config);

        let discovery = AwsTargetGroupDiscovery::new(
            elb_client,
            ec2_client,
            self.target_group_arn.clone(),
            self.prover_port,
        );

        let registry =
            RegistryContractClient::new(self.tee_prover_registry_address, self.l1_rpc_url.clone());

        let ready = Arc::new(AtomicBool::new(false));
        let health_handle = tokio::spawn(HealthServer::serve(
            self.health.socket_addr(),
            Arc::clone(&ready),
            cancel.clone(),
        ));

        let signer_client = ProverClient::new(Duration::from_secs(self.prover_timeout_secs));
        let driver_config = DriverConfig {
            poll_interval: Duration::from_secs(self.poll_interval_secs),
            cancel: cancel.clone(),
            max_concurrency: self.max_concurrency,
            unhealthy_registration_window: Duration::from_secs(
                self.unhealthy_registration_window_secs,
            ),
        };
        let signer_manager_config = SignerManagerConfig {
            registry_address: self.tee_prover_registry_address,
            max_concurrency: self.max_concurrency,
            max_tx_retries: self.max_tx_retries,
            tx_retry_delay: Duration::from_secs(self.tx_retry_delay_secs),
        };

        // Mark the service as ready. This signals "initialised and running", not
        // "connectivity verified" — the registrar is an outbound-only service that
        // does not receive traffic, so readiness gating on L1/AWS connectivity
        // would add complexity without benefit.
        ready.store(true, Ordering::SeqCst);

        let signer_manager = Arc::new(SignerManager::new(
            boundless_prover,
            registry,
            tx_manager.clone(),
            signer_manager_config,
        ));
        let cert_manager = if self.crl_check_enabled {
            let nitro_verifier_address = self
                .crl_nitro_verifier_address
                .expect("--crl-nitro-verifier-address is required by clap");
            let nitro_verifier = Box::new(NitroVerifierContractClient::new(
                nitro_verifier_address,
                self.l1_rpc_url.clone(),
            ));
            Some(CertManager::new(
                Duration::from_secs(DEFAULT_CRL_FETCH_TIMEOUT_SECS),
                nitro_verifier,
                tx_manager,
            )?)
        } else {
            None
        };
        let cancel_guard = cancel.clone().drop_guard();
        let driver = RegistrationDriver::new(
            discovery,
            signer_client,
            driver_config,
            cert_manager,
            signer_manager,
        );
        let driver_result = driver.run().await;
        drop(cancel_guard);

        info!("Driver stopped, shutting down...");
        ready.store(false, Ordering::SeqCst);
        RegistrarMetrics::up().set(0.0);

        match health_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(error = %e, "Health server error during shutdown"),
            Err(e) => warn!(error = %e, "Health server task panicked"),
        }

        signal_handle.abort();

        info!("Service stopped");
        driver_result?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const TEST_L1_RPC: &str = "http://localhost:8545";
    const TEST_L1_CHAIN_ID: &str = "1";
    const TEST_REGISTRY_ADDR: &str = "0x0000000000000000000000000000000000000001";
    const TEST_TARGET_GROUP_ARN: &str =
        "arn:aws:elasticloadbalancing:us-east-1:123456789012:targetgroup/prover/abc123";
    const TEST_AWS_REGION: &str = "us-east-1";
    const TEST_PRIVATE_KEY: &str =
        "0x0101010101010101010101010101010101010101010101010101010101010101";
    const TEST_BOUNDLESS_RPC: &str = "http://localhost:9545";
    const TEST_BOUNDLESS_KEY: &str =
        "0202020202020202020202020202020202020202020202020202020202020202";
    const TEST_VERIFIER_URL: &str = "https://gateway.pinata.cloud/ipfs/bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
    const TEST_IMAGE_ID: &str =
        "0x0100000002000000030000000400000005000000060000000700000008000000";
    const TEST_BOUNDLESS_MIN_PRICE_ETH: &str = "0.01";
    const TEST_BOUNDLESS_MAX_PRICE_ETH: &str = "0.03";
    const TEST_BOUNDLESS_RAMP_UP_PERIOD_SECS: u32 = 30;

    fn args(extra: &[&'static str]) -> Vec<&'static str> {
        let mut args = vec![
            "prover-registrar",
            "--l1-rpc-url",
            TEST_L1_RPC,
            "--l1-chain-id",
            TEST_L1_CHAIN_ID,
            "--tee-prover-registry-address",
            TEST_REGISTRY_ADDR,
            "--target-group-arn",
            TEST_TARGET_GROUP_ARN,
            "--aws-region",
            TEST_AWS_REGION,
            "--private-key",
            TEST_PRIVATE_KEY,
        ];
        args.extend(extra);
        args
    }

    fn boundless_args() -> Vec<&'static str> {
        args(&[
            "--image-id",
            TEST_IMAGE_ID,
            "--boundless-rpc-url",
            TEST_BOUNDLESS_RPC,
            "--boundless-private-key",
            TEST_BOUNDLESS_KEY,
            "--boundless-verifier-program-url",
            TEST_VERIFIER_URL,
        ])
    }

    fn boundless_prover(args: Vec<&'static str>) -> BoundlessProver {
        Cli::parse_from(args).boundless_prover().unwrap()
    }

    #[test]
    fn zero_values_fail_clap_parse() {
        for flag in [
            "--poll-interval-secs",
            "--prover-timeout-secs",
            "--boundless-timeout-secs",
            "--max-concurrency",
            "--tx-retry-delay-secs",
        ] {
            let mut args = boundless_args();
            args.extend([flag, "0"]);
            assert!(Cli::try_parse_from(args).is_err(), "{flag} should reject zero");
        }
    }

    #[test]
    fn image_id_parsed_correctly() {
        let b = boundless_prover(boundless_args());
        assert_eq!(b.image_id, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    /// `--boundless-timeout-secs` default should cover a 10-minute
    /// lock timeout with the SDK-derived `Offer.timeout = 1200 s`
    /// plus headroom.
    #[test]
    fn boundless_timeout_default_covers_default_lock_timeout() {
        let b = boundless_prover(boundless_args());

        assert_eq!(b.timeout, Duration::from_secs(1260));
    }

    #[test]
    fn boundless_offer_pricing_parses_eth_amounts() {
        let mut args = boundless_args();
        args.extend([
            "--boundless-min-price-eth",
            TEST_BOUNDLESS_MIN_PRICE_ETH,
            "--boundless-max-price-eth",
            TEST_BOUNDLESS_MAX_PRICE_ETH,
            "--boundless-offer-ramp-up-period-secs",
            "30",
        ]);

        let b = boundless_prover(args);

        assert_eq!(
            b.offer_min_price,
            Some(parse_boundless_eth_amount(TEST_BOUNDLESS_MIN_PRICE_ETH).unwrap()),
        );
        assert_eq!(
            b.offer_max_price,
            Some(parse_boundless_eth_amount(TEST_BOUNDLESS_MAX_PRICE_ETH).unwrap()),
        );
        assert_eq!(b.offer_ramp_up_period_secs, Some(TEST_BOUNDLESS_RAMP_UP_PERIOD_SECS),);
    }

    #[test]
    fn boundless_offer_min_price_requires_max_price() {
        let mut args = boundless_args();
        args.extend(["--boundless-min-price-eth", TEST_BOUNDLESS_MIN_PRICE_ETH]);

        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn boundless_offer_max_price_must_cover_min_price() {
        let mut args = boundless_args();
        args.extend([
            "--boundless-min-price-eth",
            TEST_BOUNDLESS_MAX_PRICE_ETH,
            "--boundless-max-price-eth",
            TEST_BOUNDLESS_MIN_PRICE_ETH,
        ]);

        let result = Cli::parse_from(args).boundless_prover();

        assert!(result.is_err());
    }

    #[test]
    fn parse_image_id_valid() {
        for input in [
            "0x0100000002000000030000000400000005000000060000000700000008000000",
            "0100000002000000030000000400000005000000060000000700000008000000",
        ] {
            assert_eq!(parse_image_id(input).unwrap(), [1, 2, 3, 4, 5, 6, 7, 8]);
        }
    }

    #[test]
    fn parse_image_id_invalid() {
        for input in ["00000001", "zzzz", ""] {
            assert!(parse_image_id(input).is_err());
        }
    }

    #[test]
    fn crl_enabled_without_verifier_address_fails() {
        let mut args = boundless_args();
        args.extend(["--crl-check-enabled"]);
        assert!(Cli::try_parse_from(args).is_err());
    }
}
