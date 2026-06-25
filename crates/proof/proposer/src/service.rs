//! Full proposer service lifecycle.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_primitives::Address;
use alloy_provider::{Provider, ProviderBuilder};
use base_balance_monitor::BalanceMonitorLayer;
use base_cli_utils::RuntimeManager;
use base_health::HealthServer;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, AnchorStateRegistryClient,
    AnchorStateRegistryContractClient, DisputeGameFactoryClient, DisputeGameFactoryContractClient,
};
use base_proof_rpc::{
    L1Client, L1ClientConfig, L2Client, L2ClientConfig, RollupClient, RollupClientConfig,
};
use base_prover_service_client::{
    ProofRequesterClient, ProofRequesterProvider, ProverServiceClientConfig,
};
use base_tx_manager::{BaseTxMetrics, SimpleTxManager};
use eyre::{Result, WrapErr};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    Metrics,
    config::ProposerConfig,
    driver::{DriverConfig, PipelineHandle, ProposerDriverControl},
    output_proposer::{OutputProposer, ProposalSubmitter},
    pipeline::ProvingPipeline,
    proof_collector::ProofCollector,
    proof_dispatcher::{ProofDispatcher, ProofDispatcherConfig},
    proof_recovery::{ProofRecovery, ProofRecoveryConfig},
    proof_submitter::{ProofSubmitter, ProofSubmitterConfig},
};

const SUBMIT_TIMEOUT_SLACK: Duration = Duration::from_mins(2);
const DEFAULT_TX_SEND_TIMEOUT: Duration = Duration::from_mins(10);
const DEFAULT_SUBMIT_TIMEOUT: Duration =
    Duration::from_secs(DEFAULT_TX_SEND_TIMEOUT.as_secs() + SUBMIT_TIMEOUT_SLACK.as_secs());

/// Top-level proposer service.
#[derive(Debug)]
pub struct ProposerService;

impl ProposerService {
    /// Runs the full proposer service lifecycle.
    pub async fn run(config: ProposerConfig) -> Result<()> {
        // Install the default rustls CryptoProvider before any TLS connections are created.
        // Required by rustls 0.23+ when custom TLS configs are used (e.g. skip_tls_verify).
        let _ = rustls::crypto::ring::default_provider().install_default();

        info!(version = env!("CARGO_PKG_VERSION"), "Proposer starting");
        info!(
            dry_run = config.dry_run,
            allow_non_finalized = config.allow_non_finalized,
            anchor_state_registry = %config.anchor_state_registry_addr,
            dispute_game_factory = %config.dispute_game_factory_addr,
            game_type = config.game_type,
            tee_image_hash = %config.tee_image_hash,
            prover_timeout = ?config.prover_timeout,
            poll_interval = ?config.poll_interval,
            rpc_timeout = ?config.rpc_timeout,
            health_addr = %config.health_addr,
            admin_addr = ?config.admin_addr,
            tee_prover_registry = ?config.tee_prover_registry_address,
            "Resolved configuration"
        );

        let cancel = CancellationToken::new();
        let signal_handle = RuntimeManager::install_signal_handler(cancel.clone());

        let l1_config = L1ClientConfig::new(config.l1_eth_rpc.clone())
            .with_timeout(config.rpc_timeout)
            .with_retry_config(config.retry)
            .with_skip_tls_verify(config.skip_tls_verify)
            .with_metrics_prefix("base_proposer");
        let l1_client = Arc::new(L1Client::new(l1_config)?);
        info!(endpoint = %config.l1_eth_rpc, "L1 client initialized");

        let l2_config = L2ClientConfig::new(config.l2_eth_rpc.clone())
            .with_timeout(config.rpc_timeout)
            .with_retry_config(config.retry)
            .with_skip_tls_verify(config.skip_tls_verify)
            .with_metrics_prefix("base_proposer");
        let l2_client = Arc::new(L2Client::new(l2_config)?);
        info!(endpoint = %config.l2_eth_rpc, "L2 client initialized");

        let rollup_config = RollupClientConfig::new(config.rollup_rpc.clone())
            .with_timeout(config.rpc_timeout)
            .with_retry_config(config.retry)
            .with_skip_tls_verify(config.skip_tls_verify);
        let rollup_client = Arc::new(RollupClient::new(rollup_config)?);
        info!(endpoint = %config.rollup_rpc, "Rollup client initialized");

        let prover_service_config = ProverServiceClientConfig::new(config.prover_rpc.to_string())
            .with_max_wait(config.prover_timeout);
        let proof_requester = ProofRequesterClient::connect(&prover_service_config)
            .wrap_err("failed to create prover-service requester client")?;
        let proof_requester: Arc<dyn ProofRequesterProvider> = Arc::new(proof_requester);
        info!(endpoint = %config.prover_rpc, "Prover-service requester client initialized");

        let anchor_registry: Arc<dyn AnchorStateRegistryClient> =
            Arc::new(AnchorStateRegistryContractClient::new(
                config.anchor_state_registry_addr,
                config.l1_eth_rpc.clone(),
            )?);
        info!(address = %config.anchor_state_registry_addr, "AnchorStateRegistry client initialized");

        let factory_client = DisputeGameFactoryContractClient::new(
            config.dispute_game_factory_addr,
            config.l1_eth_rpc.clone(),
        )?;
        info!(address = %config.dispute_game_factory_addr, "DisputeGameFactory client initialized");

        let verifier_client = AggregateVerifierContractClient::new(config.l1_eth_rpc.clone())?;
        let impl_address = factory_client.game_impls(config.game_type).await?;
        if impl_address == Address::ZERO {
            return Err(eyre::eyre!(
                "no AggregateVerifier implementation registered for game type {}",
                config.game_type
            ));
        }
        let (block_interval, intermediate_block_interval, init_bond) = tokio::try_join!(
            verifier_client.read_block_interval(impl_address),
            verifier_client.read_intermediate_block_interval(impl_address),
            factory_client.init_bonds(config.game_type),
        )?;
        if block_interval < 2 {
            return Err(eyre::eyre!(
                "BLOCK_INTERVAL ({block_interval}) must be at least 2; single-block proposals are not supported"
            ));
        }
        if block_interval % intermediate_block_interval != 0 {
            return Err(eyre::eyre!(
                "BLOCK_INTERVAL ({block_interval}) is not divisible by INTERMEDIATE_BLOCK_INTERVAL ({intermediate_block_interval})"
            ));
        }
        info!(
            block_interval,
            intermediate_block_interval,
            intermediate_roots_count = block_interval / intermediate_block_interval,
            init_bond = %init_bond,
            impl_address = %impl_address,
            game_type = config.game_type,
            "Read onchain config from AggregateVerifier and DisputeGameFactory"
        );

        let factory_client: Arc<dyn DisputeGameFactoryClient> = Arc::new(factory_client);
        let verifier_client: Arc<dyn AggregateVerifierClient> = Arc::new(verifier_client);
        let submit_timeout =
            config.tx_manager.as_ref().map_or(Some(DEFAULT_SUBMIT_TIMEOUT), |tx| {
                (!tx.tx_send_timeout.is_zero())
                    .then(|| tx.tx_send_timeout.saturating_add(SUBMIT_TIMEOUT_SLACK))
            });

        let (output_proposer, proposer_address): (Arc<dyn OutputProposer>, Option<Address>) =
            if config.dry_run {
                info!("Dry-run mode enabled - proofs will be sourced but NOT submitted onchain");
                (Arc::new(crate::DryRunProposer), None)
            } else {
                let signing = config.signing.ok_or_else(|| {
                    eyre::eyre!("signing config required when not in dry-run mode")
                })?;
                let tx_config = config.tx_manager.ok_or_else(|| {
                    eyre::eyre!("tx manager config required when not in dry-run mode")
                })?;

                let sender_addr = signing.address();

                let l1_tx_provider = if config.metrics.enabled {
                    let (layer, balance_rx) = BalanceMonitorLayer::new(
                        sender_addr,
                        cancel.clone(),
                        BalanceMonitorLayer::DEFAULT_POLL_INTERVAL,
                    );
                    let provider =
                        ProviderBuilder::new().layer(layer).connect_http(config.l1_eth_rpc.clone());
                    tokio::spawn(async move {
                        let mut rx = balance_rx;
                        while rx.changed().await.is_ok() {
                            Metrics::account_balance_wei().set(f64::from(*rx.borrow_and_update()));
                        }
                    });
                    info!(addr = %sender_addr, "Balance monitor started");
                    provider
                } else {
                    ProviderBuilder::new().connect_http(config.l1_eth_rpc.clone())
                };

                let l1_chain_id =
                    l1_tx_provider.get_chain_id().await.wrap_err("failed to fetch L1 chain ID")?;
                let tx_manager = SimpleTxManager::new(
                    l1_tx_provider,
                    signing,
                    tx_config,
                    l1_chain_id,
                    Arc::new(BaseTxMetrics::new("proposer")),
                )
                .await
                .wrap_err("failed to construct tx manager")?;
                info!(addr = %sender_addr, "Transaction manager initialized");

                let submitter = ProposalSubmitter::new(
                    tx_manager,
                    config.dispute_game_factory_addr,
                    config.game_type,
                    init_bond,
                );
                (Arc::new(submitter), Some(sender_addr))
            };
        info!("Output proposer initialized");

        let driver_config = DriverConfig {
            poll_interval: config.poll_interval,
            recovery_scan_concurrency: config.recovery_scan_concurrency,
            submit_timeout,
            tee_prover_registry_address: config.tee_prover_registry_address,
            block_interval,
            intermediate_block_interval,
            game_type: config.game_type,
            allow_non_finalized: config.allow_non_finalized,
            proposer_address: proposer_address.unwrap_or_default(),
            tee_image_hash: config.tee_image_hash,
            anchor_state_registry_address: config.anchor_state_registry_addr,
        };
        let proof_dispatcher = ProofDispatcher::new(
            Arc::clone(&proof_requester),
            Arc::<L1Client>::clone(&l1_client),
            Arc::<L2Client>::clone(&l2_client),
            Arc::<RollupClient>::clone(&rollup_client),
            ProofDispatcherConfig::from(&driver_config),
        );
        let proof_submitter = ProofSubmitter::new(
            output_proposer,
            Arc::clone(&rollup_client),
            Arc::clone(&l1_client),
            Arc::clone(&factory_client),
            Arc::clone(&verifier_client),
            ProofSubmitterConfig {
                proposer_address: driver_config.proposer_address,
                game_type: driver_config.game_type,
                block_interval: driver_config.block_interval,
                intermediate_block_interval: driver_config.intermediate_block_interval,
                tee_image_hash: driver_config.tee_image_hash,
                tee_prover_registry_address: driver_config.tee_prover_registry_address,
                output_fetch_concurrency: driver_config.recovery_scan_concurrency,
            },
        );
        let proof_recovery = Arc::new(ProofRecovery::new(
            ProofRecoveryConfig {
                block_interval: driver_config.block_interval,
                intermediate_block_interval: driver_config.intermediate_block_interval,
                game_type: driver_config.game_type,
                allow_non_finalized: driver_config.allow_non_finalized,
                anchor_state_registry_address: driver_config.anchor_state_registry_address,
                scan_concurrency: driver_config.recovery_scan_concurrency,
            },
            Arc::<RollupClient>::clone(&rollup_client),
            anchor_registry,
            factory_client,
        ));
        let proof_collector = ProofCollector::new(
            Arc::clone(&proof_requester),
            Arc::clone(&rollup_client),
            proof_submitter,
            driver_config.block_interval,
            driver_config.submit_timeout,
        );
        let pipeline =
            ProvingPipeline::new(driver_config, proof_dispatcher, proof_recovery, proof_collector);
        info!("Proving pipeline initialized");
        let driver_handle: Arc<dyn ProposerDriverControl> =
            Arc::new(PipelineHandle::new(pipeline, cancel.clone()));

        let ready = Arc::new(AtomicBool::new(false));
        let health_handle: JoinHandle<Result<()>> = {
            let ready = Arc::clone(&ready);
            let addr = config.health_addr;
            let health_cancel = cancel.clone();
            tokio::spawn(async move { HealthServer::serve(addr, ready, health_cancel).await })
        };

        let admin_server = if let Some(admin_addr) = config.admin_addr {
            info!("Admin RPC enabled");
            let driver = Arc::clone(&driver_handle);
            Some(crate::admin::ProposerAdminApiServerImpl::spawn(admin_addr, driver).await?)
        } else {
            None
        };

        driver_handle
            .start_proposer()
            .await
            .map_err(|e| eyre::eyre!("failed to start proposer: {e}"))?;

        ready.store(true, Ordering::SeqCst);
        Metrics::record_startup();
        info!(
            poll_interval = ?config.poll_interval,
            block_interval,
            game_type = config.game_type,
            "Service is ready"
        );

        cancel.cancelled().await;
        info!("Shutdown signal received, stopping service...");

        ready.store(false, Ordering::SeqCst);

        if driver_handle.is_running()
            && let Err(e) = driver_handle.stop_proposer().await
        {
            warn!(error = %e, "Error stopping proposer driver");
        }

        if let Some(admin_server) = admin_server {
            let _ = admin_server.stop();
            admin_server.stopped().await;
        }

        match health_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(error = %e, "Health server error during shutdown"),
            Err(e) => warn!(error = %e, "Health server task panicked"),
        }

        signal_handle.abort();
        match signal_handle.await {
            Ok(()) => {}
            Err(e) if e.is_cancelled() => {}
            Err(e) => warn!(error = %e, "Signal handler task panicked"),
        }

        info!("Service stopped");
        Ok(())
    }
}
