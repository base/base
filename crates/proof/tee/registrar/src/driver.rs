//! Registration driver — core orchestration loop.
//!
//! Discovers prover instances, checks onchain registration status, generates
//! ZK proofs for unregistered signers, and submits registration transactions
//! to L1 via the [`TxManager`]. Also detects orphaned onchain signers (those
//! no longer backed by a healthy instance) and deregisters them.

use std::{collections::HashSet, fmt, sync::Arc, time::Duration};

use alloy_primitives::{Address, hex};
use base_proof_tee_nitro_attestation_prover::AttestationProofProvider;
use base_tx_manager::TxManager;
use futures::stream::StreamExt;
use rand::random;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::{
    CertManager, InstanceDiscovery, InstanceHealthStatus, ProofTaskSet, ProverClient,
    ProverInstance, RegistrarError, RegistrarMetrics, RegistryClient, Result, SignerClient,
    SignerManager,
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

/// Runtime parameters for the [`RegistrationDriver`] that are not
/// trait-based dependencies.
#[derive(Debug, Clone)]
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
    /// Zero-based enclave index on the source instance.
    pub enclave_index: usize,
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
pub struct RegistrationDriver<D, S, P, R, T> {
    discovery: D,
    signer_client: S,
    config: DriverConfig,
    /// Certificate revocation manager.
    cert_manager: Option<CertManager<T>>,
    /// Signer lifecycle manager for registration tasks and orphan cleanup.
    signer_manager: Arc<SignerManager<P, R, T>>,
}

impl<D, S, P, R, T> fmt::Debug for RegistrationDriver<D, S, P, R, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistrationDriver").field("config", &self.config).finish_non_exhaustive()
    }
}

impl<D, S, P, R, T> RegistrationDriver<D, S, P, R, T>
where
    D: InstanceDiscovery + 'static,
    S: SignerClient + 'static,
    P: AttestationProofProvider + 'static,
    R: RegistryClient + 'static,
    T: TxManager + 'static,
{
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

    /// Runs the registration loop until cancelled.
    pub async fn run(&self) -> Result<()> {
        info!(
            poll_interval = ?self.config.poll_interval,
            max_concurrency = self.config.max_concurrency,
            "starting registration driver"
        );

        let mut proof_tasks = ProofTaskSet::new();

        loop {
            // Keep task state current before reconcile decisions each cycle.
            proof_tasks.reap_finished_tasks();

            match self.discover_and_resolve().await {
                Ok((resolution, ok_to_dereg)) => {
                    proof_tasks.reap_finished_tasks();

                    if !self.config.cancel.is_cancelled() {
                        self.signer_manager.reconcile_proof_tasks(
                            &resolution,
                            &mut proof_tasks,
                            &self.config.cancel,
                        );

                        if ok_to_dereg {
                            let active_signers = &resolution.active_signers;
                            if let Err(e) = self
                                .signer_manager
                                .run_orphan_dereg(
                                    |signer| {
                                        active_signers.contains(signer)
                                            || proof_tasks.has_pending_signer(signer)
                                    },
                                    &self.config.cancel,
                                )
                                .await
                            {
                                warn!(error = %e, "orphan deregistration pass failed");
                                RegistrarMetrics::processing_errors_total().increment(1);
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "discovery cycle failed");
                    RegistrarMetrics::processing_errors_total().increment(1);
                }
            }

            RegistrarMetrics::proof_tasks_pending().set(proof_tasks.pending_len() as f64);

            tokio::select! {
                biased;
                () = self.config.cancel.cancelled() => {
                    info!(
                        pending = proof_tasks.pending_len(),
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

        if !instance.health_status.should_register() {
            let recently_launched_unhealthy = instance.health_status
                == InstanceHealthStatus::Unhealthy
                && instance.launch_time.is_some_and(|lt| {
                    lt.elapsed()
                        .is_ok_and(|elapsed| elapsed < self.config.unhealthy_registration_window)
                });
            if !recently_launched_unhealthy {
                debug!(
                    status = ?instance.health_status,
                    instance = %instance.instance_id,
                    "instance not registerable, skipping registration"
                );
                return Ok(outcome);
            }
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
            .signer_attestation(&instance.endpoint, None, Some(nonce.to_vec()))
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

        if all_attestations.len() < addresses.len() {
            warn!(
                expected = addresses.len(),
                actual = all_attestations.len(),
                instance = %instance.instance_id,
                "signer attestation count was lower than signer public key count"
            );
            RegistrarMetrics::processing_errors_total().increment(1);
            outcome.unresolved_instance_ids.insert(instance.instance_id.clone());
            return Ok(outcome);
        }

        if self.config.cancel.is_cancelled() {
            return Ok(outcome);
        }
        if let Some(cert_manager) = &self.cert_manager {
            let first_attestation =
                all_attestations.first().ok_or_else(|| RegistrarError::ProverClient {
                    instance: instance.endpoint.to_string(),
                    source: "no attestations available for CRL check".into(),
                })?;
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

        outcome.registerable.extend(addresses.into_iter().zip(all_attestations).enumerate().map(
            |(enclave_index, (signer, attestation))| RegisterableSigner {
                instance: instance.clone(),
                signer,
                attestation,
                enclave_index,
            },
        ));
        Ok(outcome)
    }

    /// Runs one discovery cycle and resolves every instance into a [`DiscoveryResolution`].
    async fn discover_and_resolve(&self) -> Result<(DiscoveryResolution, bool)> {
        let instances = self.discovery.discover_instances().await?;
        RegistrarMetrics::discovery_success_total().increment(1);

        if !instances.is_empty() {
            let registerable_count =
                instances.iter().filter(|i| i.health_status.should_register()).count();
            info!(
                total = instances.len(),
                registerable = registerable_count,
                "discovered prover instances"
            );
        }

        let total_count = instances.len();
        let mut resolution = DiscoveryResolution::default();
        let mut reachable_count = 0usize;

        let mut futs = futures::stream::iter(instances.into_iter().map(|instance| {
            let span = info_span!(
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
                    reachable_count += 1;
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

        let ok_to_dereg = !self.config.cancel.is_cancelled()
            && resolution.unresolved_instance_ids.is_empty()
            && (total_count == 0 || reachable_count * 2 > total_count);

        if !ok_to_dereg {
            debug!(
                reachable = reachable_count,
                total = total_count,
                "skipping orphan deregistration this cycle"
            );
        }

        Ok((resolution, ok_to_dereg))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        sync::Arc,
        time::SystemTime,
    };

    use alloy_primitives::Address;
    use async_trait::async_trait;
    use base_proof_tee_nitro_attestation_prover::{AttestationProof, AttestationProofProvider};
    use tokio_util::sync::CancellationToken;
    use url::Url;

    use super::*;
    use crate::{
        DEFAULT_MAX_TX_RETRIES, DEFAULT_TX_RETRY_DELAY_SECS, InstanceHealthStatus, RegistrarError,
        RegistryClient, Result, SignerClient, SignerManagerConfig,
        test_utils::{
            EP1, EP2, EP3, EP4, HARDHAT_KEY_0, HARDHAT_KEY_1, HARDHAT_KEY_2, NoopTxManager,
            TEST_REGISTRY_ADDRESS, healthy_prover_instance, prover_instance,
            public_key_from_private, signer_from_private_key,
        },
    };

    #[async_trait]
    impl InstanceDiscovery for Vec<ProverInstance> {
        async fn discover_instances(&self) -> Result<Self> {
            Ok(self.clone())
        }
    }

    #[derive(Clone, Debug, Default)]
    struct MockSignerClient {
        keys: HashMap<Url, Vec<Vec<u8>>>,
        attestations: HashMap<Url, Vec<Vec<u8>>>,
        fail_attestation: HashSet<Url>,
        cancel_after_public_key_success: Option<CancellationToken>,
    }

    impl MockSignerClient {
        fn from_keys(entries: &[(&str, &[u8; 32])]) -> Self {
            let keys = entries
                .iter()
                .map(|(ep, pk)| {
                    let url = endpoint_url(ep);
                    (url, vec![public_key_from_private(pk)])
                })
                .collect();
            Self { keys, ..Self::default() }
        }

        fn multi_enclave(host_port: &str, private_keys: &[&[u8; 32]]) -> Self {
            let pubs = private_keys.iter().map(|pk| public_key_from_private(pk)).collect();
            Self { keys: HashMap::from([(endpoint_url(host_port), pubs)]), ..Self::default() }
        }

        fn with_attestations(mut self, host_port: &str, attestations: Vec<Vec<u8>>) -> Self {
            self.attestations.insert(endpoint_url(host_port), attestations);
            self
        }

        fn with_attestation_failure(mut self, host_port: &str) -> Self {
            self.fail_attestation.insert(endpoint_url(host_port));
            self
        }

        fn with_cancel_after_public_key_success(mut self, cancel: CancellationToken) -> Self {
            self.cancel_after_public_key_success = Some(cancel);
            self
        }
    }

    #[async_trait]
    impl SignerClient for MockSignerClient {
        async fn signer_public_key(&self, endpoint: &Url) -> Result<Vec<Vec<u8>>> {
            let result =
                self.keys.get(endpoint).cloned().ok_or_else(|| RegistrarError::ProverClient {
                    instance: endpoint.to_string(),
                    source: "unreachable".into(),
                });
            if result.is_ok()
                && let Some(cancel) = &self.cancel_after_public_key_success
            {
                cancel.cancel();
            }
            result
        }

        async fn signer_attestation(
            &self,
            endpoint: &Url,
            _user_data: Option<Vec<u8>>,
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

    #[async_trait]
    impl AttestationProofProvider for MockSignerClient {
        async fn generate_proof(
            &self,
            _attestation_bytes: &[u8],
            _cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            unreachable!("driver discover_and_resolve tests do not spawn proof tasks")
        }
    }

    #[async_trait]
    impl RegistryClient for () {
        async fn is_registered(&self, _signer: Address) -> Result<bool> {
            unreachable!("driver discover_and_resolve tests do not query registration state")
        }

        async fn get_registered_signers(&self) -> Result<Vec<Address>> {
            unreachable!("driver discover_and_resolve tests do not run orphan deregistration")
        }
    }

    fn endpoint_url(host_port: &str) -> Url {
        Url::parse(&format!("http://{host_port}")).unwrap()
    }

    fn cycle_driver(
        instances: Vec<ProverInstance>,
        signer_client: MockSignerClient,
        cancel: CancellationToken,
    ) -> RegistrationDriver<
        Vec<ProverInstance>,
        MockSignerClient,
        MockSignerClient,
        (),
        NoopTxManager,
    > {
        let config = DriverConfig {
            poll_interval: Duration::from_secs(1),
            cancel,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            unhealthy_registration_window: Duration::from_secs(
                DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS,
            ),
        };
        let signer_manager = Arc::new(SignerManager::new(
            signer_client.clone(),
            (),
            NoopTxManager,
            SignerManagerConfig {
                registry_address: TEST_REGISTRY_ADDRESS,
                max_concurrency: DEFAULT_MAX_CONCURRENCY,
                max_tx_retries: DEFAULT_MAX_TX_RETRIES,
                tx_retry_delay: Duration::from_secs(DEFAULT_TX_RETRY_DELAY_SECS),
            },
        ));

        RegistrationDriver::new(instances, signer_client, config, None, signer_manager)
    }

    #[tokio::test]
    async fn discover_and_resolve_admits_recently_launched_unhealthy_to_active_and_registerable() {
        // A recently-launched Unhealthy instance must be included in
        // `registerable` and contribute its signer to `active_signers`.
        let addr = signer_from_private_key(&HARDHAT_KEY_0);
        let launch_time = Some(SystemTime::now() - Duration::from_secs(300));

        let instance_under_test =
            prover_instance(EP1, InstanceHealthStatus::Unhealthy, launch_time);
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);

        let driver = cycle_driver(
            vec![instance_under_test.clone()],
            signer_client,
            CancellationToken::new(),
        );

        let (resolution, ok_to_dereg) = driver.discover_and_resolve().await.unwrap();
        assert_eq!(resolution.registerable.len(), 1);
        assert_eq!(resolution.registerable[0].signer, addr);
        assert!(resolution.active_signers.contains(&addr));
        assert!(ok_to_dereg);
    }

    #[tokio::test]
    async fn discover_and_resolve_allows_orphan_pass_when_discovery_is_empty() {
        let driver =
            cycle_driver(vec![], MockSignerClient::from_keys(&[]), CancellationToken::new());

        let (resolution, ok_to_dereg) = driver.discover_and_resolve().await.unwrap();
        assert!(resolution.active_signers.is_empty());
        assert!(ok_to_dereg);
    }

    #[tokio::test]
    async fn discover_and_resolve_clears_ok_to_dereg_when_cancelled_mid_resolution() {
        let cancel = CancellationToken::new();
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)])
            .with_cancel_after_public_key_success(cancel.clone());
        let driver = cycle_driver(vec![healthy_prover_instance(EP1)], signer_client, cancel);

        let (_, ok_to_dereg) = driver.discover_and_resolve().await.unwrap();

        assert!(!ok_to_dereg);
    }

    #[tokio::test]
    async fn discover_and_resolve_majority_unreachable_clears_ok_to_dereg() {
        let instances = vec![
            healthy_prover_instance(EP1),
            healthy_prover_instance(EP2),
            healthy_prover_instance(EP3),
        ];
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let driver = cycle_driver(instances, signer_client, CancellationToken::new());

        let (_, ok_to_dereg) = driver.discover_and_resolve().await.unwrap();

        assert!(!ok_to_dereg);
    }

    #[tokio::test]
    async fn discover_and_resolve_includes_all_reachable_when_one_instance_is_unreachable() {
        let unreachable = healthy_prover_instance(EP4);
        let reachable = [
            healthy_prover_instance(EP1),
            healthy_prover_instance(EP2),
            healthy_prover_instance(EP3),
        ];
        let instances = std::iter::once(unreachable.clone())
            .chain(reachable.iter().cloned())
            .collect::<Vec<_>>();

        let signer_client = MockSignerClient::from_keys(&[
            (EP1, &HARDHAT_KEY_0),
            (EP2, &HARDHAT_KEY_1),
            (EP3, &HARDHAT_KEY_2),
        ]);

        let driver = cycle_driver(instances, signer_client, CancellationToken::new());

        let (resolution, ok_to_dereg) = driver.discover_and_resolve().await.unwrap();
        assert_eq!(resolution.registerable.len(), reachable.len());
        assert_eq!(resolution.unresolved_instance_ids, HashSet::from([unreachable.instance_id]));
        assert!(!ok_to_dereg);
    }

    #[tokio::test]
    async fn discover_and_resolve_multi_enclave_draining_protects_all_signers_from_deregistration()
    {
        let addr0 = signer_from_private_key(&HARDHAT_KEY_0);
        let addr1 = signer_from_private_key(&HARDHAT_KEY_1);

        let instances = vec![prover_instance(EP1, InstanceHealthStatus::Draining, None)];
        let signer_client = MockSignerClient::multi_enclave(EP1, &[&HARDHAT_KEY_0, &HARDHAT_KEY_1]);

        let driver = cycle_driver(instances, signer_client, CancellationToken::new());

        let (resolution, ok_to_dereg) = driver.discover_and_resolve().await.unwrap();
        assert!(resolution.registerable.is_empty());
        assert!(resolution.active_signers.contains(&addr0));
        assert!(resolution.active_signers.contains(&addr1));
        assert!(ok_to_dereg);
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
            MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)]);

        let driver = cycle_driver(instances, signer_client, CancellationToken::new());

        let (resolution, ok_to_dereg) = driver.discover_and_resolve().await.unwrap();
        assert_eq!(resolution.registerable.len(), 1);
        assert_eq!(resolution.registerable[0].signer, addr_healthy);
        assert!(resolution.active_signers.contains(&addr_unhealthy));
        assert!(ok_to_dereg);
    }

    #[tokio::test]
    async fn discover_and_resolve_attestation_failure_keeps_signer_active_and_unresolved() {
        let signer_addr = signer_from_private_key(&HARDHAT_KEY_0);
        let inst = healthy_prover_instance(EP1);
        let signer_clients = [
            MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]).with_attestations(EP1, vec![]),
            MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]).with_attestation_failure(EP1),
        ];

        for signer_client in signer_clients {
            let driver = cycle_driver(vec![inst.clone()], signer_client, CancellationToken::new());

            let (resolution, ok_to_dereg) = driver.discover_and_resolve().await.unwrap();

            assert!(resolution.active_signers.contains(&signer_addr));
            assert!(resolution.registerable.is_empty());
            assert_eq!(
                resolution.unresolved_instance_ids,
                HashSet::from([inst.instance_id.clone()])
            );
            assert!(!ok_to_dereg);
        }
    }
}
