//! Registration driver — core orchestration loop.
//!
//! Observes prover endpoints and feeds completed discovery cycles to [`SignerReconciler`].
//! Delegates registration tasks and gated orphan cleanup to [`SignerManager`].

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use base_proof_contracts::{CertManagerClient, TEEProverRegistryClient};
use base_tx_manager::TxManager;
use futures::stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, warn};

use crate::{
    EnclaveEndpointClient, InstanceDiscovery, InstanceHealthStatus, ProofTaskSet, ProverClient,
    ProverInstance, RegistrarMetrics, Result, SignerManager, SignerReconciler,
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

/// Endpoint I/O outcome for one discovered instance, before applying cache grace.
#[derive(Debug)]
pub enum InstanceObservation {
    /// The readiness probe failed.
    Unreachable,
    /// Signer addresses resolved; draining instances have no registerable signers.
    Resolved {
        /// Addresses derived from the enclave public keys.
        signers: Vec<Address>,
        /// Signers with attestations eligible for registration.
        registerable: Vec<RegisterableSigner>,
    },
    /// Signer addresses resolved, but attestations failed or had the wrong count.
    AttestationsUnavailable {
        /// Addresses derived from the enclave public keys.
        signers: Vec<Address>,
    },
    /// Public key fetching or address derivation failed.
    Failed,
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
        let mut reconciler = SignerReconciler::new(self.config.instance_cache_ttl_cycles);

        loop {
            let discovery = self.discover_and_resolve().await;

            // Keep task state current before reconcile decisions each cycle.
            proof_tasks.reap_finished_tasks();

            match discovery {
                Ok(_) if self.config.cancel.is_cancelled() => {}
                Ok(None) => {}
                Ok(Some(observations)) => {
                    // Newly spawned tasks use signers already protected by the active set.
                    let resolution = reconciler.step(observations, &proof_tasks.pending);
                    self.signer_manager.reconcile_proof_tasks(
                        &resolution,
                        &mut proof_tasks,
                        &self.config.cancel,
                    );

                    if let Some(protected_signers) = &resolution.orphan_cleanup
                        && let Err(e) = self
                            .signer_manager
                            .run_orphan_dereg(protected_signers, &self.config.cancel)
                            .await
                    {
                        warn!(error = %e, "orphan deregistration pass failed");
                        RegistrarMetrics::processing_errors_total().increment(1);
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
    /// Resolves signer keys and attestations without cache grace, omitting cancelled instances.
    async fn resolve_instance(
        &self,
        instance: &ProverInstance,
    ) -> Result<Option<InstanceObservation>> {
        if self.config.cancel.is_cancelled() {
            return Ok(None);
        }

        if instance.health_status == InstanceHealthStatus::Unhealthy {
            debug!(instance = %instance.instance_id, "unhealthy instance, skipping resolution");
            return Ok(Some(InstanceObservation::Unreachable));
        }

        let public_keys = self.signer_client.signer_public_key(&instance.endpoint).await?;
        let addresses = public_keys
            .iter()
            .map(|key| ProverClient::derive_address(key))
            .collect::<Result<Vec<_>>>()?;

        if addresses.is_empty() {
            return Ok(Some(InstanceObservation::Resolved {
                signers: addresses,
                registerable: Vec::new(),
            }));
        }

        if instance.health_status != InstanceHealthStatus::Healthy {
            debug!(
                status = ?instance.health_status,
                instance = %instance.instance_id,
                "instance not registerable, skipping registration"
            );
            return Ok(Some(InstanceObservation::Resolved {
                signers: addresses,
                registerable: Vec::new(),
            }));
        }

        if self.config.cancel.is_cancelled() {
            return Ok(None);
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
                    return Ok(Some(InstanceObservation::AttestationsUnavailable {
                        signers: addresses,
                    }));
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
            return Ok(Some(InstanceObservation::AttestationsUnavailable { signers: addresses }));
        }

        let registerable = addresses
            .iter()
            .copied()
            .zip(all_attestations)
            .map(|(signer, attestation)| RegisterableSigner {
                instance: instance.clone(),
                signer,
                attestation,
            })
            .collect();
        Ok(Some(InstanceObservation::Resolved { signers: addresses, registerable }))
    }

    /// Probes and resolves instances concurrently, omitting cancelled instances.
    pub async fn observe_instances(
        &self,
        instances: Vec<ProverInstance>,
    ) -> Vec<(ProverInstance, InstanceObservation)> {
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

        let mut observations = Vec::new();
        while let Some((instance, result)) = futs.next().await {
            let Some(result) = result else {
                continue;
            };
            let observation = match result {
                Ok(Some(observation)) => observation,
                Ok(None) => continue,
                Err(e) => {
                    warn!(
                        error = %e,
                        instance = %instance.instance_id,
                        endpoint = %instance.endpoint,
                        "failed to resolve instance"
                    );
                    RegistrarMetrics::processing_errors_total().increment(1);
                    InstanceObservation::Failed
                }
            };
            observations.push((instance, observation));
        }
        observations
    }

    /// Runs discovery and endpoint I/O, returning no cycle when cancelled.
    async fn discover_and_resolve(
        &self,
    ) -> Result<Option<Vec<(ProverInstance, InstanceObservation)>>> {
        let instances = self.discovery.discover_instances().await?;
        RegistrarMetrics::discovery_success_total().increment(1);
        let observations = self.observe_instances(instances).await;
        if self.config.cancel.is_cancelled() {
            return Ok(None);
        }
        Ok(Some(observations))
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

    async fn discover_once(
        driver: &TestDriver,
    ) -> Option<Vec<(ProverInstance, InstanceObservation)>> {
        driver.discover_and_resolve().await.unwrap()
    }

    #[tokio::test]
    async fn observations_resolve_draining_signers_without_registration_io() {
        let mut client =
            MockEnclaveEndpointClient::multi_enclave(EP1, &[&HARDHAT_KEY_0, &HARDHAT_KEY_1]);
        client.fail_attestation.insert(endpoint_url(EP1));
        client.fail_readyz.insert(endpoint_url(EP1));
        let driver = cycle_driver(vec![], client, CancellationToken::new());
        let observations = driver
            .observe_instances(vec![prover_instance(EP1, InstanceHealthStatus::Draining)])
            .await;
        assert!(matches!(
            &observations[..],
            [(_, InstanceObservation::Resolved { signers, registerable })]
            if *signers == vec![
                signer_from_private_key(&HARDHAT_KEY_0),
                signer_from_private_key(&HARDHAT_KEY_1),
            ] && registerable.is_empty()
        ));
        assert!(driver.signer_client.requested_readyz.lock().unwrap().is_empty());
        assert!(driver.signer_client.requested_nonces.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn observations_omit_cancelled_draining_instance() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let driver = cycle_driver(vec![], MockEnclaveEndpointClient::default(), cancel);

        let observations = driver
            .observe_instances(vec![prover_instance(EP1, InstanceHealthStatus::Draining)])
            .await;

        assert!(
            observations.is_empty(),
            "cancelled instance produced observations: {observations:?}"
        );
    }

    #[tokio::test]
    async fn observations_report_readyz_failure() {
        let mut client = MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        client.fail_readyz.insert(endpoint_url(EP1));
        let driver = cycle_driver(vec![], client, CancellationToken::new());

        let observations = driver.observe_instances(vec![healthy_prover_instance(EP1)]).await;

        assert!(matches!(&observations[..], [(_, InstanceObservation::Unreachable)]));
        assert!(driver.signer_client.requested_public_keys.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn observations_report_attestation_failure_with_signers() {
        let mut client = MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        client.fail_attestation.insert(endpoint_url(EP1));
        let driver = cycle_driver(vec![], client, CancellationToken::new());

        let observations = driver.observe_instances(vec![healthy_prover_instance(EP1)]).await;

        assert!(matches!(
            &observations[..],
            [(_, InstanceObservation::AttestationsUnavailable { signers })]
            if *signers == vec![signer_from_private_key(&HARDHAT_KEY_0)]
        ));
    }

    #[tokio::test]
    async fn observations_report_too_few_attestations_with_signers() {
        let mut client = MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        client.attestations.insert(endpoint_url(EP1), vec![]);
        let driver = cycle_driver(vec![], client, CancellationToken::new());

        let observations = driver.observe_instances(vec![healthy_prover_instance(EP1)]).await;

        assert!(matches!(
            &observations[..],
            [(_, InstanceObservation::AttestationsUnavailable { signers })]
            if *signers == vec![signer_from_private_key(&HARDHAT_KEY_0)]
        ));
    }

    #[tokio::test]
    async fn observations_report_too_many_attestations_with_signers() {
        let mut client = MockEnclaveEndpointClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        client.attestations.insert(endpoint_url(EP1), vec![vec![1], vec![2]]);
        let driver = cycle_driver(vec![], client, CancellationToken::new());

        let observations = driver.observe_instances(vec![healthy_prover_instance(EP1)]).await;

        assert!(matches!(
            &observations[..],
            [(_, InstanceObservation::AttestationsUnavailable { signers })]
            if *signers == vec![signer_from_private_key(&HARDHAT_KEY_0)]
        ));
    }

    #[tokio::test]
    async fn observations_report_key_fetch_failure() {
        let driver =
            cycle_driver(vec![], MockEnclaveEndpointClient::default(), CancellationToken::new());

        let observations = driver.observe_instances(vec![healthy_prover_instance(EP1)]).await;

        assert!(matches!(&observations[..], [(_, InstanceObservation::Failed)]));
    }

    #[tokio::test]
    async fn observations_report_address_derivation_failure() {
        let client = MockEnclaveEndpointClient {
            keys: HashMap::from([(endpoint_url(EP1), vec![vec![0]])]),
            ..Default::default()
        };
        let driver = cycle_driver(vec![], client, CancellationToken::new());

        let observations = driver.observe_instances(vec![healthy_prover_instance(EP1)]).await;

        assert!(matches!(&observations[..], [(_, InstanceObservation::Failed)]));
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

        let observations = discover_once(&driver).await;
        let observations = observations.unwrap();
        assert_eq!(observations.len(), 4);
        assert!(
            observations
                .iter()
                .any(|(instance, observation)| instance.instance_id == unreachable.instance_id
                    && matches!(observation, InstanceObservation::Failed))
        );
        let resolved = observations.iter().filter(|(_, observation)| {
            matches!(
                observation,
                InstanceObservation::Resolved { registerable, .. }
                if registerable.len() == 1
            )
        });
        assert_eq!(resolved.count(), 3);
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

        let observations = discover_once(&driver).await;

        assert!(matches!(
            &observations.unwrap()[..],
            [(_, InstanceObservation::Resolved { registerable, .. })]
            if registerable.len() == 2
        ));
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

        let observations = discover_once(&driver).await;

        assert!(observations.is_none());
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
            resolution = &mut discovery => {
                panic!("discovery completed before cancellation: {resolution:?}");
            }
            result = tokio::time::timeout(Duration::from_secs(1), public_key_requested.notified()) => {
                result.expect("healthy instance should resolve while another readyz probe blocks");
            }
        }

        cancel.cancel();
        let resolution = tokio::time::timeout(Duration::from_secs(1), &mut discovery)
            .await
            .expect("discovery should stop after cancellation");
        assert!(resolution.is_none());
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

        let observations = discover_once(&driver).await;
        let observations = observations.unwrap();
        assert_eq!(observations.len(), 2);
        assert!(
            observations
                .iter()
                .any(|(instance, observation)| instance.instance_id == format!("i-{EP1}")
                    && matches!(observation, InstanceObservation::Unreachable))
        );
        assert!(observations.iter().any(|(_, observation)| {
            matches!(
                observation,
                InstanceObservation::Resolved { signers, registerable }
                if *signers == vec![addr_healthy] && registerable.len() == 1
                    && registerable[0].signer == addr_healthy
            )
        }));
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

        let observations = discover_once(&driver).await;
        assert!(matches!(
            &observations.unwrap()[..],
            [(_, InstanceObservation::Resolved { signers, registerable })]
            if *signers == vec![addr] && registerable.len() == 1
                && registerable[0].signer == addr
        ));
        assert_eq!(*requested_readyz.lock().unwrap(), vec![endpoint_url(EP1)]);
    }
}
