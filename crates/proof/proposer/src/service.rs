//! Full proposer service lifecycle.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_primitives::Address;
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
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
    proof_dispatcher::ProofDispatcher,
    proof_recovery::{ProofRecovery, ProofRecoveryConfig},
    proof_submitter::ProofSubmitter,
    proposal_intervals::IntervalResolver,
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
            anchor_state_registry = %config.anchor_state_registry_addr,
            dispute_game_factory = %config.dispute_game_factory_addr,
            game_type = config.game_type,
            prover_timeout = ?config.prover_timeout,
            poll_interval = ?config.poll_interval,
            rpc_timeout = ?config.rpc_timeout,
            health_addr = %config.health_addr,
            admin_addr = ?config.admin_addr,
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

        let read_provider = RootProvider::new_http(config.l1_eth_rpc.clone());
        let anchor_registry: Arc<dyn AnchorStateRegistryClient> =
            Arc::new(AnchorStateRegistryContractClient::new(
                config.anchor_state_registry_addr,
                read_provider.clone(),
            ));
        info!(address = %config.anchor_state_registry_addr, "AnchorStateRegistry client initialized");

        let factory_client = DisputeGameFactoryContractClient::new(
            config.dispute_game_factory_addr,
            read_provider.clone(),
        );
        info!(address = %config.dispute_game_factory_addr, "DisputeGameFactory client initialized");

        let verifier_client = AggregateVerifierContractClient::new(read_provider);
        let impl_address = factory_client.game_impls(config.game_type).await?;
        if impl_address == Address::ZERO {
            return Err(eyre::eyre!(
                "no AggregateVerifier implementation registered for game type {}",
                config.game_type
            ));
        }
        let init_bond = factory_client.init_bonds(config.game_type).await?;
        info!(
            init_bond = %init_bond,
            impl_address = %impl_address,
            game_type = config.game_type,
            "Read onchain config from AggregateVerifier and DisputeGameFactory"
        );

        let factory_client: Arc<dyn DisputeGameFactoryClient> = Arc::new(factory_client);
        let verifier_client: Arc<dyn AggregateVerifierClient> = Arc::new(verifier_client);

        // The proposal intervals change at the Cobalt activation block, so they are
        // resolved from each game's starting block instead of being read once here.
        let intervals = Arc::new(IntervalResolver::new(
            Arc::clone(&verifier_client),
            Arc::clone(&factory_client),
            config.game_type,
        ));
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
            game_type: config.game_type,
            proposer_address: proposer_address.unwrap_or_default(),
            anchor_state_registry_address: config.anchor_state_registry_addr,
        };
        let proof_dispatcher = ProofDispatcher::new(
            Arc::clone(&proof_requester),
            Arc::<L1Client>::clone(&l1_client),
            Arc::<L2Client>::clone(&l2_client),
            Arc::<RollupClient>::clone(&rollup_client),
            Arc::clone(&intervals),
            driver_config.proposer_address,
        );
        let proof_submitter = ProofSubmitter::new(
            output_proposer,
            Arc::<RollupClient>::clone(&rollup_client),
            Arc::clone(&factory_client),
            Arc::clone(&verifier_client),
            Arc::clone(&intervals),
            &driver_config,
        );
        let proof_recovery = Arc::new(ProofRecovery::new(
            ProofRecoveryConfig {
                game_type: driver_config.game_type,
                anchor_state_registry_address: driver_config.anchor_state_registry_address,
                scan_concurrency: driver_config.recovery_scan_concurrency,
            },
            Arc::<RollupClient>::clone(&rollup_client),
            anchor_registry,
            factory_client,
            Arc::clone(&intervals),
        ));
        let proof_collector = ProofCollector::new(
            Arc::clone(&proof_requester),
            Arc::clone(&rollup_client),
            proof_submitter,
            Arc::clone(&intervals),
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
