//! Registration driver — core orchestration loop.
//!
//! Discovers prover instances, checks onchain registration status, generates
//! ZK proofs for unregistered signers, and submits registration transactions
//! to L1 via the [`TxManager`]. Also detects orphaned onchain signers (those
//! no longer backed by a healthy instance) and deregisters them.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use alloy_primitives::{Address, hex};
use base_proof_contracts::TEEProverRegistryClient;
use base_proof_tee_nitro_attestation_prover::AttestationProofProvider;
use base_tx_manager::TxManager;
use futures::stream::StreamExt;
use rand::random;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, warn};

use crate::{
    CertManager, EnclaveEndpointClient, InstanceDiscovery, InstanceHealthStatus, ProofTaskSet,
    ProverClient, ProverInstance, RegistrarMetrics, Result, SignerManager,
};

/// Default maximum number of instances processed concurrently.
///
/// Each instance may trigger a ~20-minute Boundless proof generation, so
/// limiting concurrency prevents overwhelming the proof service and keeps
/// resource usage bounded. The transaction manager handles nonce
/// serialization separately.
pub const DEFAULT_MAX_CONCURRENCY: usize = 4;

/// Default duration (in seconds) after launch during which unhealthy
/// instances are still eligible for registration.
///
/// New EC2 instances may fail ALB health checks while the application is
/// still initializing. This window allows the registrar to attempt
/// registration during that warm-up period rather than waiting for the
/// instance to become healthy. Set to 0 to disable.
///
/// 85 minutes gives a slight buffer ahead of the prove provision timeout
/// of 90 minutes.
pub const DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS: u64 = 5100;

/// Number of consecutive discovery cycles to protect last-known active signers
/// for an instance that disappears from otherwise successful discovery output.
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
    /// Duration after launch during which unhealthy instances are still
    /// eligible for registration. New instances may fail ALB health checks
    /// while the application is still initializing. Set to zero to disable.
    /// Defaults to [`DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS`] seconds.
    pub unhealthy_registration_window: Duration,
}

/// A signer and attestation ready to be spawned as a proof task.
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
pub struct RegistrationDriver<D, S, P, R, T> {
    discovery: D,
    signer_client: S,
    config: DriverConfig,
    /// Certificate revocation manager.
    cert_manager: Option<CertManager<T>>,
    /// Signer lifecycle manager for registration tasks and orphan cleanup.
    signer_manager: Arc<SignerManager<P, R, T>>,
}

impl<D, S, P, R, T> RegistrationDriver<D, S, P, R, T> {
    /// Creates a new registration driver.
    ///
    /// Accepts a pre-built certificate manager so CRL client construction and
    /// revocation transaction wiring stay outside the core driver loop.
    pub const fn new(
        discovery: D,
        signer_client: S,
        config: DriverConfig,
        cert_manager: Option<CertManager<T>>,
        signer_manager: Arc<SignerManager<P, R, T>>,
    ) -> Self {
        Self { discovery, signer_client, config, cert_manager, signer_manager }
    }
}

impl<D, S, P, R, T> RegistrationDriver<D, S, P, R, T>
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
        P: AttestationProofProvider + 'static,
        R: TEEProverRegistryClient + 'static,
        T: 'static,
    {
        info!(
            poll_interval = ?self.config.poll_interval,
            max_concurrency = self.config.max_concurrency,
            "starting registration driver"
        );

        let mut proof_tasks = ProofTaskSet::default();
        let mut last_known_active = HashMap::new();

        loop {
            let discovery = self.discover_and_resolve(&mut last_known_active).await;

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

        let recently_launched_unhealthy = instance.health_status == InstanceHealthStatus::Unhealthy
            && instance.launch_time.is_some_and(|lt| {
                lt.elapsed()
                    .is_ok_and(|elapsed| elapsed < self.config.unhealthy_registration_window)
            });
        if !matches!(
            instance.health_status,
            InstanceHealthStatus::Initial | InstanceHealthStatus::Healthy
        ) && !recently_launched_unhealthy
        {
            debug!(
                status = ?instance.health_status,
                instance = %instance.instance_id,
                "instance not registerable, skipping registration"
            );
            return Ok(outcome);
        }
        if recently_launched_unhealthy {
            info!(
                instance = %instance.instance_id,
                launch_time = ?instance.launch_time,
                window = ?self.config.unhealthy_registration_window,
                "unhealthy instance recently launched, attempting registration"
            );
        }

        if self.config.cancel.is_cancelled() {
            return Ok(outcome);
        }

        let nonce: [u8; 32] = random();
        info!(
            nonce = %hex::encode(nonce),
            instance = %instance.instance_id,
            "requesting attestations with nonce"
        );
        let all_attestations = match self
            .signer_client
            .signer_attestation(&instance.endpoint, Some(nonce.to_vec()))
            .await
        {
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

        if self.config.cancel.is_cancelled() {
            return Ok(outcome);
        }
        if let Some(cert_manager) = &self.cert_manager {
            let first_attestation = all_attestations
                .first()
                .expect("guarded by attestation count == signer count >= 1");
            match cert_manager.check_and_revoke_crls(first_attestation, instance).await {
                Ok(true) => {
                    warn!(
                        instance = %instance.instance_id,
                        "certificate revoked, skipping registration for this instance"
                    );
                    return Ok(outcome);
                }
                Ok(false) => {}
                Err(e) => {
                    warn!(
                        error = %e,
                        instance = %instance.instance_id,
                        "CRL check failed (fail-open, proceeding with registration)"
                    );
                }
            }
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
    ) -> Result<DiscoveryResolution> {
        let instances = self.discovery.discover_instances().await?;
        RegistrarMetrics::discovery_success_total().increment(1);

        let discovered_instance_ids: HashSet<String> =
            instances.iter().map(|instance| instance.instance_id.clone()).collect();
        let mut resolution = DiscoveryResolution::default();

        let mut futs = futures::stream::iter(instances.into_iter().map(|instance| {
            let span = tracing::info_span!(
                "resolve_instance",
                instance_id = %instance.instance_id,
                endpoint = %instance.endpoint,
                health = ?instance.health_status,
            );
            async move {
                let result = self.resolve_instance(&instance).await;
                (instance, result)
            }
            .instrument(span)
        }))
        .buffer_unordered(self.config.max_concurrency.max(1));

        // No cancel-select around `futs.next()`: each future checks
        // cancellation cooperatively between awaits, so new work is
        // short-circuited while already-started resolution work reaches a
        // natural boundary.
        while let Some((instance, result)) = futs.next().await {
            match result {
                Ok(outcome) => {
                    let active_signers = outcome.active_signers.iter().copied().collect::<Vec<_>>();
                    if active_signers.is_empty() {
                        last_known_active.remove(&instance.instance_id);
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

        last_known_active.retain(|instance_id, (addresses, ttl_cycles)| {
            if discovered_instance_ids.contains(instance_id) {
                return true;
            }

            *ttl_cycles = ttl_cycles.saturating_add(1);
            if *ttl_cycles <= INSTANCE_CACHE_TTL_CYCLES {
                warn!(
                    instance = %instance_id,
                    cached_signers = addresses.len(),
                    ttl_cycles = *ttl_cycles,
                    max_ttl_cycles = INSTANCE_CACHE_TTL_CYCLES,
                    "instance missing from discovery, preserving last-known active signers"
                );
                resolution.active_signers.extend(addresses.iter().copied());
                resolution.unresolved_instance_ids.insert(instance_id.clone());
                true
            } else {
                warn!(
                    instance = %instance_id,
                    max_ttl_cycles = INSTANCE_CACHE_TTL_CYCLES,
                    "last-known active signer cache expired for missing instance"
                );
                false
            }
        });

        Ok(resolution)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        sync::Arc,
        time::SystemTime,
    };

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
    }

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
        async fn signer_public_key(&self, endpoint: &Url) -> Result<Vec<Vec<u8>>> {
            self.keys.get(endpoint).cloned().ok_or_else(|| RegistrarError::ProverClient {
                instance: endpoint.to_string(),
                source: "unreachable".into(),
            })
        }

        async fn signer_attestation(
            &self,
            endpoint: &Url,
            _nonce: Option<Vec<u8>>,
        ) -> Result<Vec<Vec<u8>>> {
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

    type TestDriver = RegistrationDriver<
        Vec<ProverInstance>,
        MockEnclaveEndpointClient,
        MockEnclaveEndpointClient,
        (),
        NoopTxManager,
    >;

    const TEST_MAX_ATTESTATION_AGE: Duration = Duration::from_secs(3300);

    fn endpoint_url(host_port: &str) -> Url {
        Url::parse(&format!("http://{host_port}")).unwrap()
    }

    fn cycle_driver(
        instances: Vec<ProverInstance>,
        signer_client: MockEnclaveEndpointClient,
        cancel: CancellationToken,
    ) -> TestDriver {
        let signer_manager = Arc::new(SignerManager::new(
            signer_client.clone(),
            (),
            NoopTxManager,
            SignerManagerConfig {
                registry_address: TEST_REGISTRY_ADDRESS,
                max_concurrency: DEFAULT_MAX_CONCURRENCY,
                max_tx_retries: DEFAULT_MAX_TX_RETRIES,
                tx_retry_delay: Duration::from_secs(DEFAULT_TX_RETRY_DELAY_SECS),
                max_attestation_age: TEST_MAX_ATTESTATION_AGE,
            },
        ));

        RegistrationDriver::new(
            instances,
            signer_client,
            DriverConfig {
                poll_interval: Duration::from_secs(1),
                cancel,
                max_concurrency: DEFAULT_MAX_CONCURRENCY,
                unhealthy_registration_window: Duration::from_secs(
                    DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS,
                ),
            },
            None,
            signer_manager,
        )
    }

    async fn discover_once(driver: &TestDriver) -> DiscoveryResolution {
        let mut last_known_active = HashMap::new();
        driver.discover_and_resolve(&mut last_known_active).await.unwrap()
    }

    #[tokio::test]
    async fn discover_and_resolve_admits_recently_launched_unhealthy_to_active_and_registerable() {
        let addr = signer_from_private_key(&HARDHAT_KEY_0);
        let launch_time = Some(SystemTime::now() - Duration::from_secs(300));

        let instance_under_test =
            prover_instance(EP1, InstanceHealthStatus::Unhealthy, launch_time);
        let signer_client = MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);

        let driver = cycle_driver(
            vec![instance_under_test.clone()],
            signer_client,
            CancellationToken::new(),
        );

        let resolution = discover_once(&driver).await;
        assert_eq!(resolution.registerable.len(), 1);
        assert_eq!(resolution.registerable[0].signer, addr);
        assert!(resolution.active_signers.contains(&addr));
        assert!(resolution.unresolved_instance_ids.is_empty());
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

        let instances = vec![prover_instance(EP1, InstanceHealthStatus::Draining, None)];
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
    async fn discover_and_resolve_unhealthy_instance_is_reachable_but_not_registerable() {
        let addr_unhealthy = signer_from_private_key(&HARDHAT_KEY_0);
        let addr_healthy = signer_from_private_key(&HARDHAT_KEY_1);

        let instances = vec![
            prover_instance(EP1, InstanceHealthStatus::Unhealthy, None),
            healthy_prover_instance(EP2),
        ];

        let signer_client =
            MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)]);

        let driver = cycle_driver(instances, signer_client, CancellationToken::new());

        let resolution = discover_once(&driver).await;
        assert_eq!(resolution.registerable.len(), 1);
        assert_eq!(resolution.registerable[0].signer, addr_healthy);
        assert!(resolution.active_signers.contains(&addr_unhealthy));
        assert!(resolution.unresolved_instance_ids.is_empty());
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
    async fn discover_and_resolve_evicts_cached_missing_instance_after_ttl() {
        let signer_addr = signer_from_private_key(&HARDHAT_KEY_0);
        let inst = healthy_prover_instance(EP1);
        let signer_client = MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let first_cycle =
            cycle_driver(vec![inst.clone()], signer_client.clone(), CancellationToken::new());
        let missing_cycle = cycle_driver(vec![], signer_client, CancellationToken::new());
        let mut last_known_active = HashMap::new();

        first_cycle.discover_and_resolve(&mut last_known_active).await.unwrap();

        for expected_ttl in 1..=INSTANCE_CACHE_TTL_CYCLES {
            let resolution =
                missing_cycle.discover_and_resolve(&mut last_known_active).await.unwrap();

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

        let expired_resolution =
            missing_cycle.discover_and_resolve(&mut last_known_active).await.unwrap();

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

        present_cycle.discover_and_resolve(&mut last_known_active).await.unwrap();
        missing_cycle.discover_and_resolve(&mut last_known_active).await.unwrap();
        assert_eq!(last_known_active.get(&inst.instance_id).map(|(_, ttl)| *ttl), Some(1));

        let refresh_resolution =
            present_cycle.discover_and_resolve(&mut last_known_active).await.unwrap();

        assert!(refresh_resolution.active_signers.contains(&signer_addr));
        assert!(refresh_resolution.unresolved_instance_ids.is_empty());
        assert_eq!(last_known_active.get(&inst.instance_id).map(|(_, ttl)| *ttl), Some(0));

        for expected_ttl in 1..=INSTANCE_CACHE_TTL_CYCLES {
            let resolution =
                missing_cycle.discover_and_resolve(&mut last_known_active).await.unwrap();

            assert!(resolution.active_signers.contains(&signer_addr));
            assert_eq!(
                last_known_active.get(&inst.instance_id).map(|(_, ttl)| *ttl),
                Some(expected_ttl)
            );
        }

        let expired_resolution =
            missing_cycle.discover_and_resolve(&mut last_known_active).await.unwrap();

        assert!(expired_resolution.active_signers.is_empty());
        assert!(expired_resolution.unresolved_instance_ids.is_empty());
    }
}
