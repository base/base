//! Registration driver — core orchestration loop.
//!
//! Discovers prover instances, checks onchain registration status, prepares
//! hinted attestations for unregistered signers, and submits registration transactions
//! to L1 via the [`TxManager`]. Also detects orphaned onchain signers (those
//! no longer backed by a healthy instance) and deregisters them.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use alloy_primitives::Address;
use base_proof_contracts::{CertManagerClient, TEEProverRegistryClient};
use base_tx_manager::TxManager;
use futures::stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, warn};

use crate::{
    EnclaveEndpointClient, InstanceDiscovery, InstanceHealthStatus, ProofTaskSet, ProverClient,
    ProverInstance, RegistrarMetrics, Result, SignerManager,
};

/// Default maximum number of instances processed concurrently.
///
/// Each instance may trigger CPU-intensive P-384 hint generation, so limiting
/// concurrency keeps resource usage bounded. The transaction manager handles nonce
/// serialization separately.
pub const DEFAULT_MAX_CONCURRENCY: usize = 4;

/// Default number of consecutive discovery cycles to protect last-known active
/// signers for an instance that disappears from discovery or becomes unhealthy,
/// and to defer orphan cleanup for newly observed unhealthy instances.
///
/// Five cycles is roughly 2.5 minutes with the default 30 second poll interval.
/// A shorter window is more vulnerable to transient discovery flakes; a longer
/// window delays cleanup when an instance was genuinely removed.
pub const INSTANCE_CACHE_TTL_CYCLES: u32 = 5;

/// Runtime parameters for the [`RegistrationDriver`] that are not
/// trait-based dependencies.
#[derive(Debug)]
pub struct DriverConfig {
    /// Interval between discovery and registration poll cycles.
    pub poll_interval: Duration,
    /// Cancellation token for graceful shutdown.
    pub cancel: CancellationToken,
    /// Maximum number of instances resolved concurrently per discovery cycle.
    pub max_concurrency: usize,
    /// Number of consecutive discovery cycles to protect last-known active
    /// signers for an instance missing from discovery or reported unhealthy,
    /// and to defer orphan cleanup for newly observed unhealthy instances.
    /// Defaults to [`INSTANCE_CACHE_TTL_CYCLES`].
    pub instance_cache_ttl_cycles: u32,
}

/// A signer and attestation ready to be spawned as a registration task.
#[derive(Debug, Clone)]
pub struct RegisterableSigner {
    /// Source prover instance for attribution.
    pub instance: ProverInstance,
    /// Signer address derived from an enclave public key.
    pub signer: Address,
    /// Pre-fetched attestation blob for the signer.
    pub attestation: Vec<u8>,
}

/// Per-cycle discovery snapshot consumed by signer reconciliation.
#[derive(Debug, Default)]
pub struct DiscoveryResolution {
    /// Signers eligible for registration this cycle.
    pub registerable: Vec<RegisterableSigner>,
    /// Signers contributed by reachable instances.
    pub active_signers: HashSet<Address>,
    /// Instance IDs whose resolution was inconclusive this cycle.
    pub unresolved_instance_ids: HashSet<String>,
}

/// Core registration loop tying together discovery, attestation polling, signer
/// lifecycle reconciliation, and orphan cleanup.
///
/// Generic over discovery and RPC backends.
#[derive(Debug)]
pub struct RegistrationDriver<D, S, R, C, T> {
    discovery: D,
    signer_client: S,
    config: DriverConfig,
    /// Signer lifecycle manager for registration tasks and orphan cleanup.
    signer_manager: Arc<SignerManager<R, C, T>>,
}

impl<D, S, R, C, T> RegistrationDriver<D, S, R, C, T> {
    /// Creates a new registration driver.
    ///
    pub const fn new(
        discovery: D,
        signer_client: S,
        config: DriverConfig,
        signer_manager: Arc<SignerManager<R, C, T>>,
    ) -> Self {
        Self { discovery, signer_client, config, signer_manager }
    }
}

impl<D, S, R, C, T> RegistrationDriver<D, S, R, C, T>
where
    D: InstanceDiscovery,
    S: EnclaveEndpointClient,
    T: TxManager,
{
    /// Runs the registration loop until cancelled.
    pub async fn run(&self) -> Result<()>
    where
        D: 'static,
        S: 'static,
        R: TEEProverRegistryClient + 'static,
        C: CertManagerClient + 'static,
        T: 'static,
    {
        info!(
            poll_interval = ?self.config.poll_interval,
            max_concurrency = self.config.max_concurrency,
            instance_cache_ttl_cycles = self.config.instance_cache_ttl_cycles,
            "starting registration driver"
        );

        let mut proof_tasks = ProofTaskSet::default();
        let mut last_known_active = HashMap::new();
        let mut unhealthy_instance_ids_with_grace = HashSet::new();

        loop {
            let discovery = self
                .discover_and_resolve(
                    &mut last_known_active,
                    &mut unhealthy_instance_ids_with_grace,
                )
                .await;

            // Keep task state current before reconcile decisions each cycle.
            proof_tasks.reap_finished_tasks();

            match discovery {
                Ok(_) if self.config.cancel.is_cancelled() => {}
                Ok(resolution) => {
                    self.signer_manager.reconcile_proof_tasks(
                        &resolution,
                        &mut proof_tasks,
                        &self.config.cancel,
                    );

                    if resolution.unresolved_instance_ids.is_empty() {
                        let mut protected_signers = resolution.active_signers.clone();
                        protected_signers.extend(proof_tasks.pending.keys().copied());
                        if let Err(e) = self
                            .signer_manager
                            .run_orphan_dereg(&protected_signers, &self.config.cancel)
                            .await
                        {
                            warn!(error = %e, "orphan deregistration pass failed");
                            RegistrarMetrics::processing_errors_total().increment(1);
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "discovery cycle failed");
                    RegistrarMetrics::processing_errors_total().increment(1);
                }
            }

            RegistrarMetrics::proof_tasks_pending().set(proof_tasks.pending.len() as f64);

            tokio::select! {
                biased;
                () = self.config.cancel.cancelled() => {
                    info!(
                        pending = proof_tasks.pending.len(),
                        "registration driver received shutdown signal"
                    );
                    break;
                }
                () = tokio::time::sleep(self.config.poll_interval) => {}
            }
        }

        proof_tasks.drain_proof_tasks().await;

        info!("registration driver stopped");
        Ok(())
    }
    /// Resolves one instance into active and registerable signers.
    async fn resolve_instance(&self, instance: &ProverInstance) -> Result<DiscoveryResolution> {
        if self.config.cancel.is_cancelled() {
            return Ok(DiscoveryResolution::default());
        }

        if instance.health_status == InstanceHealthStatus::Unhealthy {
            debug!(instance = %instance.instance_id, "unhealthy instance, skipping resolution");
            return Ok(DiscoveryResolution::default());
        }

        let public_keys = self.signer_client.signer_public_key(&instance.endpoint).await?;
        let addresses = public_keys
            .iter()
            .map(|key| ProverClient::derive_address(key))
            .collect::<Result<Vec<_>>>()?;
        let mut outcome = DiscoveryResolution {
            active_signers: addresses.iter().copied().collect(),
            ..Default::default()
        };

        if addresses.is_empty() {
            return Ok(outcome);
        }

        if instance.health_status != InstanceHealthStatus::Healthy {
            debug!(
                status = ?instance.health_status,
                instance = %instance.instance_id,
                "instance not registerable, skipping registration"
            );
            return Ok(outcome);
        }

        if self.config.cancel.is_cancelled() {
            return Ok(outcome);
        }

        let nonces = addresses
            .iter()
            .map(|signer| self.signer_manager.attestation_nonce(*signer).to_vec())
            .collect::<Vec<_>>();
        info!(
            signer_count = addresses.len(),
            instance = %instance.instance_id,
            "requesting attestations with deterministic nonces"
        );
        let all_attestations =
            match self.signer_client.signer_attestation(&instance.endpoint, Some(nonces)).await {
                Ok(attestations) => attestations,
                Err(e) => {
                    warn!(
                        error = %e,
                        instance = %instance.instance_id,
                        "failed to fetch signer attestations after resolving signer addresses"
                    );
                    RegistrarMetrics::processing_errors_total().increment(1);
                    outcome.unresolved_instance_ids.insert(instance.instance_id.clone());
                    return Ok(outcome);
                }
            };

        if all_attestations.len() != addresses.len() {
            warn!(
                expected = addresses.len(),
                actual = all_attestations.len(),
                instance = %instance.instance_id,
                "signer attestation count did not match signer public key count"
            );
            RegistrarMetrics::processing_errors_total().increment(1);
            outcome.unresolved_instance_ids.insert(instance.instance_id.clone());
            return Ok(outcome);
        }

        outcome.registerable.extend(addresses.into_iter().zip(all_attestations).map(
            |(signer, attestation)| RegisterableSigner {
                instance: instance.clone(),
                signer,
                attestation,
            },
        ));
        Ok(outcome)
    }

    /// Runs one discovery cycle and resolves every instance into a [`DiscoveryResolution`].
    async fn discover_and_resolve(
        &self,
        last_known_active: &mut HashMap<String, (Vec<Address>, u32)>,
        unhealthy_instance_ids_with_grace: &mut HashSet<String>,
    ) -> Result<DiscoveryResolution> {
        let instances = self.discovery.discover_instances().await?;
        RegistrarMetrics::discovery_success_total().increment(1);

        let discovered_instance_ids: HashSet<String> =
            instances.iter().map(|instance| instance.instance_id.clone()).collect();
        let mut resolution = DiscoveryResolution::default();
        let mut unhealthy_instance_ids = HashSet::new();

        // Probe each non-draining target immediately before resolving it. ALB
        // `/healthz` is registration-gated on nitro-host, so trusting it alone
        // deadlocks bootstrap. Pairing the probe with resolution lets healthy
        // targets progress while an unrelated probe waits for its timeout.
        let mut futs =
            futures::stream::iter(instances.into_iter().map(|mut instance| async move {
                if instance.health_status != InstanceHealthStatus::Draining {
                    let readyz = tokio::select! {
                        biased;
                        () = self.config.cancel.cancelled() => return (instance, None),
                        result = self.signer_client.readyz(&instance.endpoint) => result,
                    };
                    instance.health_status = match readyz {
                        Ok(()) => InstanceHealthStatus::Healthy,
                        Err(e) => {
                            debug!(
                                error = %e,
                                instance = %instance.instance_id,
                                endpoint = %instance.endpoint,
                                "readyz probe failed"
                            );
                            InstanceHealthStatus::Unhealthy
                        }
                    };
                }
                let span = tracing::info_span!(
                    "registrar.resolve_instance",
                    instance_id = %instance.instance_id,
                    endpoint = %instance.endpoint,
                    health = ?instance.health_status,
                );
                let result = self.resolve_instance(&instance).instrument(span).await;
                (instance, Some(result))
            }))
            .buffer_unordered(self.config.max_concurrency.max(1));

        while let Some((instance, result)) = futs.next().await {
            let Some(result) = result else {
                continue;
            };
            if instance.health_status == InstanceHealthStatus::Unhealthy {
                unhealthy_instance_ids.insert(instance.instance_id.clone());
            }
            match result {
                Ok(outcome) => {
                    let active_signers = outcome.active_signers.iter().copied().collect::<Vec<_>>();
                    if active_signers.is_empty() {
                        if instance.health_status != InstanceHealthStatus::Unhealthy {
                            last_known_active.remove(&instance.instance_id);
                        }
                    } else {
                        last_known_active.insert(instance.instance_id, (active_signers, 0));
                    }
                    resolution.registerable.extend(outcome.registerable);
                    resolution.active_signers.extend(outcome.active_signers);
                    resolution.unresolved_instance_ids.extend(outcome.unresolved_instance_ids);
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        instance = %instance.instance_id,
                        endpoint = %instance.endpoint,
                        "failed to resolve instance"
                    );
                    RegistrarMetrics::processing_errors_total().increment(1);
                    resolution.unresolved_instance_ids.insert(instance.instance_id);
                }
            }
        }

        if self.config.cancel.is_cancelled() {
            return Ok(DiscoveryResolution::default());
        }

        unhealthy_instance_ids_with_grace
            .retain(|instance_id| unhealthy_instance_ids.contains(instance_id));
        for instance_id in &unhealthy_instance_ids {
            // A process restart has no signer addresses to cache. Keep an
            // empty cache entry for one unhealthy period so the global orphan
            // cleanup pass is deferred by the configured grace TTL.
            if unhealthy_instance_ids_with_grace.insert(instance_id.clone())
                && !last_known_active.contains_key(instance_id)
            {
                last_known_active.insert(instance_id.clone(), (Vec::new(), 0));
            }
        }
        RegistrarMetrics::discovered_instances_count().set(discovered_instance_ids.len() as f64);

        last_known_active.retain(|instance_id, (addresses, ttl_cycles)| {
            if discovered_instance_ids.contains(instance_id)
                && !unhealthy_instance_ids.contains(instance_id)
            {
                return true;
            }

            *ttl_cycles = ttl_cycles.saturating_add(1);
            if *ttl_cycles <= self.config.instance_cache_ttl_cycles {
                warn!(
                    instance = %instance_id,
                    cached_signers = addresses.len(),
                    ttl_cycles = *ttl_cycles,
                    max_ttl_cycles = self.config.instance_cache_ttl_cycles,
                    "instance unavailable, preserving last-known active signers"
                );
                resolution.active_signers.extend(addresses.iter().copied());
                if addresses.is_empty() {
                    // We cannot identify signers to protect for an unhealthy
                    // instance first observed after a restart.
                    resolution.unresolved_instance_ids.insert(instance_id.clone());
                } else if !unhealthy_instance_ids.contains(instance_id) {
                    // A missing instance could have changed signers since its
                    // cached addresses were last refreshed.
                    resolution.unresolved_instance_ids.insert(instance_id.clone());
                }
                true
            } else {
                warn!(
                    instance = %instance_id,
                    ttl_cycles = *ttl_cycles,
                    max_ttl_cycles = self.config.instance_cache_ttl_cycles,
                    "last-known active signer cache expired for unavailable instance"
                );
                false
            }
        });

        RegistrarMetrics::active_signers_count().set(resolution.active_signers.len() as f64);
        RegistrarMetrics::registerable_signers_count().set(resolution.registerable.len() as f64);
        RegistrarMetrics::unresolved_instances_count()
            .set(resolution.unresolved_instance_ids.len() as f64);

        Ok(resolution)
    }
}

#[cfg(test)]
mod tests {
    //! Driver tests use a hand-rolled endpoint client because they coordinate
    //! scripted responses with a blocked readiness request across concurrent
    //! calls.

    use std::{
        collections::{HashMap, HashSet},
        sync::{Arc, Mutex},
    };

    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;
    use url::Url;

    use super::*;
    use crate::{
        DEFAULT_MAX_TX_RETRIES, DEFAULT_TX_RETRY_DELAY_SECS, EnclaveEndpointClient,
        InstanceHealthStatus, RegistrarError, Result, SignerManagerConfig,
        test_utils::{
            EP1, EP2, EP3, HARDHAT_KEY_0, HARDHAT_KEY_1, HARDHAT_KEY_2, NoopTxManager,
            TEST_REGISTRY_ADDRESS, healthy_prover_instance, prover_instance,
            public_key_from_private, signer_from_private_key,
        },
    };

    impl InstanceDiscovery for Vec<ProverInstance> {
        async fn discover_instances(&self) -> Result<Self> {
            Ok(self.clone())
        }
    }

    #[derive(Clone, Debug, Default)]
    struct MockEnclaveEndpointClient {
        keys: HashMap<Url, Vec<Vec<u8>>>,
        attestations: HashMap<Url, Vec<Vec<u8>>>,
        fail_attestation: HashSet<Url>,
        fail_readyz: HashSet<Url>,
        block_readyz: HashMap<Url, Arc<Notify>>,
        public_key_requested: Arc<Notify>,
        requested_public_keys: RequestedEndpoints,
        requested_nonces: RequestedNonces,
        requested_readyz: RequestedEndpoints,
    }

    type RequestedEndpoints = Arc<Mutex<Vec<Url>>>;
    type RequestedNonces = Arc<Mutex<Vec<Option<Vec<Vec<u8>>>>>>;

    impl MockEnclaveEndpointClient {
        fn from_keys(entries: &[(&str, &[u8; 32])]) -> Self {
            let keys = entries
                .iter()
                .map(|(ep, pk)| (endpoint_url(ep), vec![public_key_from_private(pk)]))
                .collect();
            Self { keys, ..Self::default() }
        }

        fn multi_enclave(host_port: &str, private_keys: &[&[u8; 32]]) -> Self {
            let pubs = private_keys.iter().map(|pk| public_key_from_private(pk)).collect();
            Self { keys: HashMap::from([(endpoint_url(host_port), pubs)]), ..Self::default() }
        }
    }

    impl EnclaveEndpointClient for MockEnclaveEndpointClient {
        async fn readyz(&self, endpoint: &Url) -> Result<()> {
            self.requested_readyz.lock().unwrap().push(endpoint.clone());
            if let Some(blocker) = self.block_readyz.get(endpoint) {
                blocker.notified().await;
            }
            if self.fail_readyz.contains(endpoint) {
                return Err(RegistrarError::ProverClient {
                    instance: endpoint.to_string(),
                    source: "readyz unavailable".into(),
                });
            }
            Ok(())
        }

        async fn signer_public_key(&self, endpoint: &Url) -> Result<Vec<Vec<u8>>> {
            self.requested_public_keys.lock().unwrap().push(endpoint.clone());
            self.public_key_requested.notify_one();
            self.keys.get(endpoint).cloned().ok_or_else(|| RegistrarError::ProverClient {
                instance: endpoint.to_string(),
                source: "unreachable".into(),
            })
        }

        async fn signer_attestation(
            &self,
            endpoint: &Url,
            nonces: Option<Vec<Vec<u8>>>,
        ) -> Result<Vec<Vec<u8>>> {
            self.requested_nonces.lock().unwrap().push(nonces);
            if self.fail_attestation.contains(endpoint) {
                return Err(RegistrarError::ProverClient {
                    instance: endpoint.to_string(),
                    source: "attestation unavailable".into(),
                });
            }
            if let Some(atts) = self.attestations.get(endpoint) {
                return Ok(atts.clone());
            }
            let count = self.keys.get(endpoint).map_or(1, |k| k.len());
            Ok(vec![b"mock-attestation".to_vec(); count])
        }
    }

    type TestDriver =
        RegistrationDriver<Vec<ProverInstance>, MockEnclaveEndpointClient, (), (), NoopTxManager>;

    const TEST_MAX_ATTESTATION_AGE: Duration = Duration::from_secs(3300);

    fn endpoint_url(host_port: &str) -> Url {
        Url::parse(&format!("http://{host_port}")).unwrap()
    }

    fn cycle_driver(
        instances: Vec<ProverInstance>,
        signer_client: MockEnclaveEndpointClient,
        cancel: CancellationToken,
    ) -> TestDriver {
        cycle_driver_with_instance_cache_ttl(
            instances,
            signer_client,
            cancel,
            INSTANCE_CACHE_TTL_CYCLES,
        )
    }

    fn cycle_driver_with_instance_cache_ttl(
        instances: Vec<ProverInstance>,
        signer_client: MockEnclaveEndpointClient,
        cancel: CancellationToken,
        instance_cache_ttl_cycles: u32,
    ) -> TestDriver {
        let signer_manager = Arc::new(
            SignerManager::new(
                (),
                (),
                NoopTxManager,
                SignerManagerConfig {
                    registry_address: TEST_REGISTRY_ADDRESS,
                    max_concurrency: DEFAULT_MAX_CONCURRENCY,
                    max_tx_retries: DEFAULT_MAX_TX_RETRIES,
                    tx_retry_delay: Duration::from_secs(DEFAULT_TX_RETRY_DELAY_SECS),
                    max_attestation_age: TEST_MAX_ATTESTATION_AGE,
                    crl_checks_enabled: false,
                },
            )
            .unwrap(),
        );

        RegistrationDriver::new(
            instances,
            signer_client,
            DriverConfig {
                poll_interval: Duration::from_secs(1),
                cancel,
                max_concurrency: DEFAULT_MAX_CONCURRENCY,
                instance_cache_ttl_cycles,
            },
            signer_manager,
        )
    }

    async fn discover_once(driver: &TestDriver) -> DiscoveryResolution {
        let mut last_known_active = HashMap::new();
        let mut unhealthy_instance_ids_with_grace = HashSet::new();
        driver
            .discover_and_resolve(&mut last_known_active, &mut unhealthy_instance_ids_with_grace)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn discover_and_resolve_allows_orphan_pass_when_discovery_is_empty() {
        let driver = cycle_driver(
            vec![],
            MockEnclaveEndpointClient::from_keys(&[]),
            CancellationToken::new(),
        );

        let resolution = discover_once(&driver).await;
        assert!(resolution.active_signers.is_empty());
        assert!(resolution.unresolved_instance_ids.is_empty());
    }

    #[tokio::test]
    async fn discover_and_resolve_includes_all_reachable_when_one_instance_is_unreachable() {
        let unreachable = healthy_prover_instance("10.0.0.4:8000");
        let instances = vec![
            unreachable.clone(),
            healthy_prover_instance(EP1),
            healthy_prover_instance(EP2),
            healthy_prover_instance(EP3),
        ];

        let signer_client = MockEnclaveEndpointClient::from_keys(&[
            (EP1, &HARDHAT_KEY_0),
            (EP2, &HARDHAT_KEY_1),
            (EP3, &HARDHAT_KEY_2),
        ]);

        let driver = cycle_driver(instances, signer_client, CancellationToken::new());

        let resolution = discover_once(&driver).await;
        assert_eq!(resolution.registerable.len(), 3);
        assert_eq!(resolution.unresolved_instance_ids, HashSet::from([unreachable.instance_id]));
    }

    #[tokio::test]
    async fn discover_and_resolve_multi_enclave_draining_protects_all_signers_from_deregistration()
    {
        let addr0 = signer_from_private_key(&HARDHAT_KEY_0);
        let addr1 = signer_from_private_key(&HARDHAT_KEY_1);

        let instances = vec![prover_instance(EP1, InstanceHealthStatus::Draining)];
        let signer_client =
            MockEnclaveEndpointClient::multi_enclave(EP1, &[&HARDHAT_KEY_0, &HARDHAT_KEY_1]);

        let driver = cycle_driver(instances, signer_client, CancellationToken::new());

        let resolution = discover_once(&driver).await;
        assert!(resolution.registerable.is_empty());
        assert!(resolution.active_signers.contains(&addr0));
        assert!(resolution.active_signers.contains(&addr1));
        assert!(resolution.unresolved_instance_ids.is_empty());
    }

    #[tokio::test]
    async fn discover_and_resolve_requests_deterministic_nonce_per_signer() {
        let signer_a = signer_from_private_key(&HARDHAT_KEY_0);
        let signer_b = signer_from_private_key(&HARDHAT_KEY_1);
        let signer_client =
            MockEnclaveEndpointClient::multi_enclave(EP1, &[&HARDHAT_KEY_0, &HARDHAT_KEY_1]);
        let requested_nonces = Arc::clone(&signer_client.requested_nonces);

        let driver = cycle_driver(
            vec![healthy_prover_instance(EP1)],
            signer_client,
            CancellationToken::new(),
        );

        let resolution = discover_once(&driver).await;

        assert_eq!(resolution.registerable.len(), 2);
        let nonce_a = SignerManager::<(), (), NoopTxManager>::attestation_nonce_for(
            TEST_REGISTRY_ADDRESS,
            signer_a,
        )
        .to_vec();
        let nonce_b = SignerManager::<(), (), NoopTxManager>::attestation_nonce_for(
            TEST_REGISTRY_ADDRESS,
            signer_b,
        )
        .to_vec();
        assert_eq!(*requested_nonces.lock().unwrap(), vec![Some(vec![nonce_a, nonce_b])]);
    }

    #[tokio::test]
    async fn discover_and_resolve_skips_readyz_when_cancelled() {
        let cancel = CancellationToken::new();
        let signer_client = MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let requested_readyz = Arc::clone(&signer_client.requested_readyz);
        let driver =
            cycle_driver(vec![healthy_prover_instance(EP1)], signer_client, cancel.clone());

        cancel.cancel();

        let resolution = discover_once(&driver).await;

        assert!(resolution.registerable.is_empty());
        assert!(resolution.active_signers.is_empty());
        assert!(resolution.unresolved_instance_ids.is_empty());
        assert!(requested_readyz.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn discover_and_resolve_pipelines_readyz_with_instance_resolution() {
        let cancel = CancellationToken::new();
        let mut signer_client =
            MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)]);
        signer_client.block_readyz.insert(endpoint_url(EP1), Arc::new(Notify::new()));
        let public_key_requested = Arc::clone(&signer_client.public_key_requested);
        let driver = cycle_driver(
            vec![healthy_prover_instance(EP1), healthy_prover_instance(EP2)],
            signer_client,
            cancel.clone(),
        );
        let discovery = discover_once(&driver);
        tokio::pin!(discovery);

        tokio::select! {
            resolution = &mut discovery => panic!("discovery completed before cancellation: {resolution:?}"),
            result = tokio::time::timeout(Duration::from_secs(1), public_key_requested.notified()) => {
                result.expect("healthy instance should resolve while another readyz probe blocks");
            }
        }

        cancel.cancel();
        let resolution = tokio::time::timeout(Duration::from_secs(1), &mut discovery)
            .await
            .expect("discovery should stop after cancellation");
        assert!(resolution.registerable.is_empty());
        assert!(resolution.active_signers.is_empty());
        assert!(resolution.unresolved_instance_ids.is_empty());
    }

    #[tokio::test]
    async fn discover_and_resolve_skips_instances_that_fail_readyz() {
        let addr_healthy = signer_from_private_key(&HARDHAT_KEY_1);

        let instances = vec![
            prover_instance(EP1, InstanceHealthStatus::Unhealthy),
            healthy_prover_instance(EP2),
        ];

        let mut signer_client =
            MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)]);
        signer_client.fail_readyz.insert(endpoint_url(EP1));
        let requested_public_keys = Arc::clone(&signer_client.requested_public_keys);
        let requested_readyz = Arc::clone(&signer_client.requested_readyz);

        let driver = cycle_driver(instances, signer_client, CancellationToken::new());

        let resolution = discover_once(&driver).await;
        assert_eq!(resolution.registerable.len(), 1);
        assert_eq!(resolution.registerable[0].signer, addr_healthy);
        assert!(!resolution.active_signers.contains(&signer_from_private_key(&HARDHAT_KEY_0)));
        assert_eq!(resolution.unresolved_instance_ids, HashSet::from([format!("i-{EP1}")]));
        assert_eq!(*requested_public_keys.lock().unwrap(), vec![endpoint_url(EP2)]);
        let mut probed = requested_readyz.lock().unwrap().clone();
        probed.sort();
        assert_eq!(probed, vec![endpoint_url(EP1), endpoint_url(EP2)]);
    }

    #[tokio::test]
    async fn discover_and_resolve_registers_alb_unhealthy_instance_when_readyz_passes() {
        let addr = signer_from_private_key(&HARDHAT_KEY_0);
        let instances = vec![prover_instance(EP1, InstanceHealthStatus::Unhealthy)];
        let signer_client = MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let requested_readyz = Arc::clone(&signer_client.requested_readyz);

        let driver = cycle_driver(instances, signer_client, CancellationToken::new());

        let resolution = discover_once(&driver).await;
        assert_eq!(resolution.registerable.len(), 1);
        assert_eq!(resolution.registerable[0].signer, addr);
        assert!(resolution.active_signers.contains(&addr));
        assert!(resolution.unresolved_instance_ids.is_empty());
        assert_eq!(*requested_readyz.lock().unwrap(), vec![endpoint_url(EP1)]);
    }

    #[tokio::test]
    async fn discover_and_resolve_defers_orphan_dereg_for_initial_unhealthy_instance() {
        const TEST_TTL_CYCLES: u32 = 2;

        let instance = prover_instance(EP1, InstanceHealthStatus::Unhealthy);
        let mut signer_client = MockEnclaveEndpointClient::default();
        signer_client.fail_readyz.insert(endpoint_url(EP1));
        let requested_public_keys = Arc::clone(&signer_client.requested_public_keys);
        let driver = cycle_driver_with_instance_cache_ttl(
            vec![instance.clone()],
            signer_client,
            CancellationToken::new(),
            TEST_TTL_CYCLES,
        );
        let mut last_known_active = HashMap::new();
        let mut unhealthy_instance_ids_with_grace = HashSet::new();

        for expected_ttl in 1..=TEST_TTL_CYCLES {
            let resolution = driver
                .discover_and_resolve(
                    &mut last_known_active,
                    &mut unhealthy_instance_ids_with_grace,
                )
                .await
                .unwrap();

            assert!(resolution.active_signers.is_empty());
            assert_eq!(
                resolution.unresolved_instance_ids,
                HashSet::from([instance.instance_id.clone()])
            );
            assert_eq!(
                last_known_active.get(&instance.instance_id).map(|(_, ttl)| *ttl),
                Some(expected_ttl)
            );
        }

        let resolution = driver
            .discover_and_resolve(&mut last_known_active, &mut unhealthy_instance_ids_with_grace)
            .await
            .unwrap();

        assert!(resolution.unresolved_instance_ids.is_empty());
        assert!(!last_known_active.contains_key(&instance.instance_id));

        let resolution = driver
            .discover_and_resolve(&mut last_known_active, &mut unhealthy_instance_ids_with_grace)
            .await
            .unwrap();

        assert!(resolution.unresolved_instance_ids.is_empty());
        assert!(requested_public_keys.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn discover_and_resolve_preserves_cached_signers_for_unhealthy_instances_until_ttl() {
        const TEST_TTL_CYCLES: u32 = 2;

        let signer = signer_from_private_key(&HARDHAT_KEY_0);
        let instance = prover_instance(EP1, InstanceHealthStatus::Unhealthy);
        let mut signer_client = MockEnclaveEndpointClient::default();
        signer_client.fail_readyz.insert(endpoint_url(EP1));
        let requested_public_keys = Arc::clone(&signer_client.requested_public_keys);
        let driver = cycle_driver_with_instance_cache_ttl(
            vec![instance.clone()],
            signer_client,
            CancellationToken::new(),
            TEST_TTL_CYCLES,
        );
        let mut last_known_active =
            HashMap::from([(instance.instance_id.clone(), (vec![signer], 0))]);
        let mut unhealthy_instance_ids_with_grace = HashSet::new();

        for expected_ttl in 1..=TEST_TTL_CYCLES {
            let resolution = driver
                .discover_and_resolve(
                    &mut last_known_active,
                    &mut unhealthy_instance_ids_with_grace,
                )
                .await
                .unwrap();

            assert!(resolution.active_signers.contains(&signer));
            assert!(resolution.unresolved_instance_ids.is_empty());
            assert_eq!(
                last_known_active.get(&instance.instance_id).map(|(_, ttl)| *ttl),
                Some(expected_ttl)
            );
        }

        let resolution = driver
            .discover_and_resolve(&mut last_known_active, &mut unhealthy_instance_ids_with_grace)
            .await
            .unwrap();

        assert!(resolution.active_signers.is_empty());
        assert!(resolution.unresolved_instance_ids.is_empty());
        assert!(!last_known_active.contains_key(&instance.instance_id));
        assert!(requested_public_keys.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn discover_and_resolve_bad_attestations_keep_signer_active_and_unresolved() {
        let signer_addr = signer_from_private_key(&HARDHAT_KEY_0);
        let inst = healthy_prover_instance(EP1);
        let mut missing_attestation =
            MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        missing_attestation.attestations.insert(endpoint_url(EP1), vec![]);
        let mut extra_attestation = MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        extra_attestation
            .attestations
            .insert(endpoint_url(EP1), vec![b"mock-attestation".to_vec(), b"extra".to_vec()]);
        let mut failing_attestation =
            MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        failing_attestation.fail_attestation.insert(endpoint_url(EP1));
        let signer_clients = [
            ("missing attestation", missing_attestation),
            ("extra attestation", extra_attestation),
            ("failing attestation", failing_attestation),
        ];

        for (case, signer_client) in signer_clients {
            let driver = cycle_driver(vec![inst.clone()], signer_client, CancellationToken::new());

            let resolution = discover_once(&driver).await;

            assert!(resolution.active_signers.contains(&signer_addr), "{case}");
            assert!(resolution.registerable.is_empty(), "{case}");
            assert_eq!(
                resolution.unresolved_instance_ids,
                HashSet::from([inst.instance_id.clone()]),
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn discover_and_resolve_evicts_cached_missing_instance_after_configured_ttl() {
        const TEST_TTL_CYCLES: u32 = 2;

        let signer_addr = signer_from_private_key(&HARDHAT_KEY_0);
        let inst = healthy_prover_instance(EP1);
        let signer_client = MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let first_cycle = cycle_driver_with_instance_cache_ttl(
            vec![inst.clone()],
            signer_client.clone(),
            CancellationToken::new(),
            TEST_TTL_CYCLES,
        );
        let missing_cycle = cycle_driver_with_instance_cache_ttl(
            vec![],
            signer_client,
            CancellationToken::new(),
            TEST_TTL_CYCLES,
        );
        let mut last_known_active = HashMap::new();
        let mut unhealthy_instance_ids_with_grace = HashSet::new();

        first_cycle
            .discover_and_resolve(&mut last_known_active, &mut unhealthy_instance_ids_with_grace)
            .await
            .unwrap();

        for expected_ttl in 1..=TEST_TTL_CYCLES {
            let resolution = missing_cycle
                .discover_and_resolve(
                    &mut last_known_active,
                    &mut unhealthy_instance_ids_with_grace,
                )
                .await
                .unwrap();

            assert!(resolution.registerable.is_empty());
            assert!(resolution.active_signers.contains(&signer_addr));
            assert_eq!(
                resolution.unresolved_instance_ids,
                HashSet::from([inst.instance_id.clone()])
            );
            assert_eq!(
                last_known_active.get(&inst.instance_id).map(|(_, ttl)| *ttl),
                Some(expected_ttl)
            );
        }

        let expired_resolution = missing_cycle
            .discover_and_resolve(&mut last_known_active, &mut unhealthy_instance_ids_with_grace)
            .await
            .unwrap();

        assert!(expired_resolution.active_signers.is_empty());
        assert!(expired_resolution.unresolved_instance_ids.is_empty());
        assert!(!last_known_active.contains_key(&inst.instance_id));
    }

    #[tokio::test]
    async fn discover_and_resolve_refresh_resets_cached_missing_instance_ttl() {
        let signer_addr = signer_from_private_key(&HARDHAT_KEY_0);
        let inst = healthy_prover_instance(EP1);
        let signer_client = MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let present_cycle =
            cycle_driver(vec![inst.clone()], signer_client.clone(), CancellationToken::new());
        let missing_cycle = cycle_driver(vec![], signer_client, CancellationToken::new());
        let mut last_known_active = HashMap::new();
        let mut unhealthy_instance_ids_with_grace = HashSet::new();

        present_cycle
            .discover_and_resolve(&mut last_known_active, &mut unhealthy_instance_ids_with_grace)
            .await
            .unwrap();
        missing_cycle
            .discover_and_resolve(&mut last_known_active, &mut unhealthy_instance_ids_with_grace)
            .await
            .unwrap();
        assert_eq!(last_known_active.get(&inst.instance_id).map(|(_, ttl)| *ttl), Some(1));

        let refresh_resolution = present_cycle
            .discover_and_resolve(&mut last_known_active, &mut unhealthy_instance_ids_with_grace)
            .await
            .unwrap();

        assert!(refresh_resolution.active_signers.contains(&signer_addr));
        assert!(refresh_resolution.unresolved_instance_ids.is_empty());
        assert_eq!(last_known_active.get(&inst.instance_id).map(|(_, ttl)| *ttl), Some(0));

        for expected_ttl in 1..=INSTANCE_CACHE_TTL_CYCLES {
            let resolution = missing_cycle
                .discover_and_resolve(
                    &mut last_known_active,
                    &mut unhealthy_instance_ids_with_grace,
                )
                .await
                .unwrap();

            assert!(resolution.active_signers.contains(&signer_addr));
            assert_eq!(
                last_known_active.get(&inst.instance_id).map(|(_, ttl)| *ttl),
                Some(expected_ttl)
            );
        }

        let expired_resolution = missing_cycle
            .discover_and_resolve(&mut last_known_active, &mut unhealthy_instance_ids_with_grace)
            .await
            .unwrap();

        assert!(expired_resolution.active_signers.is_empty());
        assert!(expired_resolution.unresolved_instance_ids.is_empty());
    }
}
