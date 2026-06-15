//! Registration driver — core orchestration loop.
//!
//! Discovers prover instances, checks onchain registration status, generates
//! ZK proofs for unregistered signers, and submits registration transactions
//! to L1 via the [`TxManager`]. Also detects orphaned onchain signers (those
//! no longer backed by a healthy instance) and deregisters them.

use std::{collections::HashSet, fmt, time::Duration};

use alloy_primitives::{Address, hex};
use base_tx_manager::TxManager;
use futures::stream::StreamExt;
use rand::random;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::{
    CertManager, CrlConfig, InstanceDiscovery, InstanceHealthStatus, ProofTaskSet, ProverClient,
    ProverInstance, RegistrarError, RegistrarMetrics, Result, SignerClient, SignerLifecycle,
    SignerManagerConfig,
};

/// Default maximum number of instances processed concurrently.
///
/// Each instance may trigger a ~20-minute Boundless proof generation, so
/// limiting concurrency prevents overwhelming the proof service and keeps
/// resource usage bounded. The transaction manager handles nonce
/// serialization separately.
pub const DEFAULT_MAX_CONCURRENCY: usize = 4;

/// Default maximum number of transaction submission retries for transient
/// errors before giving up.
pub const DEFAULT_MAX_TX_RETRIES: u32 = 3;

/// Default delay between transaction submission retries.
pub const DEFAULT_TX_RETRY_DELAY_SECS: u64 = 5;

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
    /// Signer lifecycle settings.
    ///
    /// The registrar currently uses `signer_manager.max_concurrency` for both
    /// per-cycle instance resolution and spawned proof task concurrency so one
    /// CLI setting controls total signer lifecycle pressure.
    pub signer_manager: SignerManagerConfig,
    /// Duration after launch during which unhealthy instances are still
    /// eligible for registration. New instances may fail ALB health checks
    /// while the application is still initializing. Set to zero to disable.
    /// Defaults to [`DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS`] seconds.
    pub unhealthy_registration_window: Duration,
    /// CRL checking configuration. When enabled, intermediate certificates
    /// are checked against CRL distribution points before registration.
    pub crl: CrlConfig,
}

/// A single (signer, attestation) pair from a prover instance that
/// passed all per-cycle gates and is ready to be spawned as a proof task.
///
/// One [`RegisterableSigner`] corresponds to exactly one spawned proof
/// task. Instances with multiple enclaves are flattened into one entry
/// per enclave at construction time in `discover_and_resolve`, so the
/// spawn pass in `reconcile_proof_tasks` is a flat iteration with no
/// per-entry index correlation between parallel vectors.
///
/// Replaces the earlier 3-tuple `(ProverInstance, Vec<Address>,
/// Vec<Vec<u8>>)` whose unnamed positional fields made the
/// `attestations[idx]` indexing contract invisible at the call site.
#[derive(Debug, Clone)]
pub struct RegisterableSigner {
    /// Source prover instance, retained so per-signer log lines and
    /// `PendingRegistration::instance_id` can attribute the spawned
    /// task. Cloned per enclave on the source instance (typically N=1)
    /// at flatten time.
    pub instance: ProverInstance,
    /// Signer address derived from one of the instance's enclave public
    /// keys. Each address gets its own spawned proof task.
    pub signer: Address,
    /// Pre-fetched attestation blob paired with [`Self::signer`] at
    /// flatten time.
    pub attestation: Vec<u8>,
    /// Zero-based enclave index on the source instance, preserved from
    /// the original `(addresses, attestations)` enumeration so per-task
    /// log lines can attribute which enclave on a multi-enclave instance
    /// the signer came from.
    pub enclave_index: usize,
}

/// Aggregate output of the driver's per-cycle `discover_and_resolve` pass —
/// the snapshot consumed by the spawn-and-reap loop.
#[derive(Debug, Default)]
pub struct DiscoveryResolution {
    /// Instances eligible for registration this cycle, with their derived
    /// signer addresses and the matching pre-fetched attestation blobs
    /// (one per enclave on the instance). Instances whose certificates
    /// were confirmed revoked by the CRL check are filtered out.
    pub registerable: Vec<RegisterableSigner>,
    /// All signers contributed by *reachable* instances, regardless of
    /// register-eligibility. Used to protect draining/unhealthy
    /// instances from premature orphan deregistration.
    pub active_signers: HashSet<Address>,
    /// Number of discovered instances that responded to discovery RPCs.
    pub reachable_count: usize,
    /// Total instances returned by discovery (reachable + unreachable).
    pub total_count: usize,
    /// Whether orphan deregistration is safe to run this cycle. `true`
    /// when the cancellation token has not fired, no discovered instance
    /// had an inconclusive resolution, **and either** `total_count` is zero
    /// (legitimate fleet drain) **or** a strict majority of discovered
    /// instances were reachable (`reachable * 2 > total`). `false` during
    /// shutdown (to avoid acquiring nonces we don't intend to broadcast),
    /// when any instance resolution was inconclusive, or when too few
    /// instances responded for the quorum guard to clear.
    pub ok_to_dereg: bool,
    /// Instance IDs whose resolution was inconclusive this cycle, either
    /// because `resolve_instance` returned `Err` or because it returned
    /// `Ok` with `unresolved: true`. In-flight proof tasks for these
    /// instances must be preserved until a later cycle reaches a verdict.
    pub unresolved_instance_ids: HashSet<String>,
}

/// Per-instance result of address resolution and registration-eligibility
/// gating. `attestations` is `Some` only when the instance is registerable
/// and its CRL check did not flag revocation; otherwise it is `None` and
/// the caller skips registration but still uses `addresses` for the
/// active-signer set. `unresolved` distinguishes intentional skips
/// (draining, old unhealthy, revoked, shutdown) from partial failures after
/// signer addresses were known (for example, attestation RPC failure).
#[derive(Debug)]
pub struct ResolveOutcome {
    /// Signer addresses derived from the instance's enclave public keys.
    pub addresses: Vec<Address>,
    /// Pre-fetched attestation blobs when the instance is register-eligible
    /// this cycle; `None` when registration is being skipped (health
    /// status, CRL revocation, shutdown).
    pub attestations: Option<Vec<Vec<u8>>>,
    /// Whether resolution reached an inconclusive state after signer
    /// addresses were already known.
    ///
    /// The caller should still add [`Self::addresses`] to the active set,
    /// because the instance proved it is reachable and advertising those
    /// signers, but should also preserve any in-flight proof task tied to
    /// the instance because registration eligibility could not be decided
    /// this cycle.
    pub unresolved: bool,
}

/// Core registration loop tying together discovery, attestation polling, signer
/// lifecycle reconciliation, and orphan cleanup.
///
/// Generic over discovery, signer client, and signer lifecycle backends so each
/// can be mocked independently in tests.
pub struct RegistrationDriver<D, S, M, T> {
    discovery: D,
    signer_client: S,
    config: DriverConfig,
    /// Certificate revocation manager. The driver only calls it when CRL
    /// checking is enabled.
    cert_manager: CertManager<T>,
    /// Signer lifecycle manager for registration tasks and orphan cleanup.
    signer_manager: M,
}

impl<D, S, M, T> fmt::Debug for RegistrationDriver<D, S, M, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistrationDriver").field("config", &self.config).finish_non_exhaustive()
    }
}

impl<D, S, M, T> RegistrationDriver<D, S, M, T>
where
    D: InstanceDiscovery + 'static,
    S: SignerClient + 'static,
    M: SignerLifecycle + 'static,
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
        cert_manager: CertManager<T>,
        signer_manager: M,
    ) -> Self {
        Self { discovery, signer_client, config, cert_manager, signer_manager }
    }

    /// Runs the registration loop until cancelled.
    ///
    /// # Pipeline
    ///
    /// Each cycle is non-blocking with respect to in-flight proof
    /// generation. Discovery, reconcile, and orphan cleanup all execute
    /// in the foreground; proofs run in dedicated `JoinSet` tasks owned
    /// by `pending`:
    ///
    /// 1. **Reap** — drain any task that finished since the previous
    ///    cycle (`reap_finished_tasks`).
    /// 2. **Discover & resolve** — produce a [`DiscoveryResolution`]
    ///    snapshot (`discover_and_resolve`).
    /// 3. **Reconcile** — cancel in-flight tasks for vanished /
    ///    ineligible signers, spawn new tasks for registerable signers
    ///    that are not already in-flight (`reconcile_proof_tasks`).
    /// 4. **Orphan dereg** — when the snapshot's `ok_to_dereg` is set,
    ///    run a single deregistration pass over signers no longer backed
    ///    by an active instance (`run_orphan_dereg`). The protected set
    ///    is the union of `resolution.active_signers` and the keys of
    ///    `pending` (see [`ProofTaskSet::protected_signers`]) so a signer
    ///    registered mid-cycle by a preserved task — whose source
    ///    instance failed `resolve_instance` transiently — cannot be
    ///    deregistered in the same pass.
    /// 5. **Sleep** — wait `poll_interval` or until cancelled.
    ///
    /// # Cancellation
    ///
    /// On shutdown every `PendingRegistration::cancel` is fired cooperatively;
    /// tasks are then awaited to natural completion via
    /// `join_next_with_id` so each terminal outcome flows through
    /// the signer manager join-outcome path, keeping the proof-task metrics
    /// consistent. `JoinSet::abort_all` is deliberately **not** used — see
    /// [`ProofTaskSet::drain_proof_tasks`] for the nonce-gap rationale.
    ///
    pub async fn run(self) -> Result<()> {
        self.run_loop().await
    }

    async fn run_loop(&self) -> Result<()> {
        info!(
            poll_interval = ?self.config.poll_interval,
            registry = %self.config.signer_manager.registry_address,
            "starting registration driver"
        );

        let mut proof_tasks = ProofTaskSet::new();

        loop {
            // Reap before discovery so finished tasks don't linger in
            // `pending` for an entire cycle and (incorrectly) cause
            // reconcile to skip spawning a replacement on transient
            // failure (audit finding #9).
            proof_tasks.reap_finished_tasks();

            match self.discover_and_resolve().await {
                Ok(resolution) => {
                    // Reap again: a task that finished during the
                    // (potentially slow) discovery RPCs would otherwise
                    // look in-flight to reconcile and get spuriously
                    // re-cancelled or have its respawn deferred a cycle.
                    proof_tasks.reap_finished_tasks();

                    // Spawning new proof tasks during a shutdown would
                    // acquire L1 nonces we have no intention of
                    // broadcasting. Skip reconcile (and the orphan
                    // dereg pass) entirely when cancellation is set.
                    if !self.config.cancel.is_cancelled() {
                        self.signer_manager.reconcile_proof_tasks(
                            &resolution,
                            &mut proof_tasks,
                            &self.config.cancel,
                        );
                    }

                    if resolution.ok_to_dereg && !self.config.cancel.is_cancelled() {
                        // Pending proof tasks can register while orphan cleanup
                        // is running, so protect those signers too.
                        let protected = proof_tasks.protected_signers(&resolution);
                        if let Err(e) = self
                            .signer_manager
                            .run_orphan_dereg(&protected, &self.config.cancel)
                            .await
                        {
                            warn!(error = %e, "orphan deregistration pass failed");
                            RegistrarMetrics::processing_errors_total().increment(1);
                        }
                    } else if !resolution.ok_to_dereg {
                        debug!(
                            reachable = resolution.reachable_count,
                            total = resolution.total_count,
                            "skipping orphan deregistration this cycle"
                        );
                    }
                }
                Err(e) => {
                    warn!(error = %e, "discovery cycle failed");
                    RegistrarMetrics::processing_errors_total().increment(1);
                }
            }

            // Publish gauge once per cycle, after every path that could
            // mutate `pending` (reconcile, reap) has run.
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

    /// Returns `true` if the instance is [`InstanceHealthStatus::Unhealthy`]
    /// and was launched within the configured
    /// [`DriverConfig::unhealthy_registration_window`].
    ///
    /// New EC2 instances may fail ALB health checks while the application is
    /// still initializing. This predicate lets the registrar attempt
    /// registration during that warm-up period rather than waiting for the
    /// instance to become healthy.
    ///
    /// Returns `false` if:
    /// - The instance is not `Unhealthy` (other statuses have their own rules).
    /// - The window is zero (feature disabled).
    /// - The instance has no launch time (e.g. discovery didn't return one).
    /// - The launch time is in the future (clock skew — treated as unknown).
    fn is_recently_launched_unhealthy(&self, instance: &ProverInstance) -> bool {
        if instance.health_status != InstanceHealthStatus::Unhealthy {
            return false;
        }
        if self.config.unhealthy_registration_window.is_zero() {
            return false;
        }
        instance.launch_time.is_some_and(|lt| {
            lt.elapsed().is_ok_and(|elapsed| elapsed < self.config.unhealthy_registration_window)
        })
    }

    /// Resolves the signer addresses for a single instance and decides
    /// whether registration should be attempted this cycle.
    ///
    /// Always returns `addresses` so the caller can track the instance's
    /// signers in the active set (protecting them from orphan
    /// deregistration even when registration is skipped). The
    /// `attestations` field is `Some` only when registration should be
    /// attempted; it is `None` when:
    ///
    /// - the instance is not register-eligible (e.g. `Draining`, or
    ///   `Unhealthy` outside the
    ///   [`DriverConfig::unhealthy_registration_window`]);
    /// - the CRL check confirmed revocation for the instance's chain.
    ///
    /// This is the shared resolution path used by `discover_and_resolve`.
    /// It keeps discovery and eligibility checks separate from the
    /// [`RegistrationManager`] registration path so long proof work can run in
    /// spawned tasks instead of blocking the next discovery cycle.
    ///
    /// **Cancellation contract.** This future checks
    /// `self.config.cancel.is_cancelled()` before starting new side effects.
    /// `check_and_revoke_crls` awaits any `revokeCert` transaction
    /// submissions it triggers, so revocation outcomes are logged before
    /// the resolution future returns.
    async fn resolve_instance(&self, instance: &ProverInstance) -> Result<ResolveOutcome> {
        if self.config.cancel.is_cancelled() {
            return Ok(ResolveOutcome {
                addresses: Vec::new(),
                attestations: None,
                unresolved: false,
            });
        }

        let public_keys = self.signer_client.signer_public_key(&instance.endpoint).await?;
        let mut addresses = Vec::with_capacity(public_keys.len());
        for public_key in &public_keys {
            addresses.push(ProverClient::derive_address(public_key)?);
        }

        if addresses.is_empty() {
            return Ok(ResolveOutcome { addresses, attestations: None, unresolved: false });
        }

        if !instance.health_status.should_register() {
            if !self.is_recently_launched_unhealthy(instance) {
                debug!(
                    status = ?instance.health_status,
                    instance = %instance.instance_id,
                    "instance not registerable, skipping registration"
                );
                return Ok(ResolveOutcome { addresses, attestations: None, unresolved: false });
            }
            info!(
                instance = %instance.instance_id,
                launch_time = ?instance.launch_time,
                window = ?self.config.unhealthy_registration_window,
                "unhealthy instance recently launched, attempting registration"
            );
        }

        if self.config.cancel.is_cancelled() {
            return Ok(ResolveOutcome { addresses, attestations: None, unresolved: false });
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
                return Ok(ResolveOutcome { addresses, attestations: None, unresolved: true });
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
            return Ok(ResolveOutcome { addresses, attestations: None, unresolved: true });
        }

        if self.config.crl.enabled {
            // Skip the CRL check and any `revokeCert` submission on shutdown
            // so we do not start new onchain work.
            //
            // Return `attestations: None` so the safety invariant —
            // `Some(..)` ↔ "passed every eligibility + security gate,
            // including CRL" — is enforced locally. Today the outer
            // run loop re-checks `cancel.is_cancelled()` before
            // calling `reconcile_proof_tasks`, so a `Some(..)` here
            // would still be discarded; keeping it `None` removes the
            // non-local dependence on that re-check and matches the
            // CRL-revoked branch below.
            if self.config.cancel.is_cancelled() {
                return Ok(ResolveOutcome { addresses, attestations: None, unresolved: false });
            }
            // Use `.first()` rather than `[0]` so the non-empty
            // invariant is locally visible: the `addresses.is_empty()`
            // early-return and the
            // `all_attestations.len() < addresses.len()` length check
            // above already guarantee at least one element, but those
            // sites are 65+ lines upstream. Surfacing the `Option`
            // here keeps `resolve_instance` indexing-panic-free even
            // if a future refactor relaxes either upstream guard.
            let first_attestation =
                all_attestations.first().ok_or_else(|| RegistrarError::ProverClient {
                    instance: instance.endpoint.to_string(),
                    source: "no attestations available for CRL check".into(),
                })?;
            match self.cert_manager.check_and_revoke_crls(first_attestation, instance).await {
                Ok(true) => {
                    warn!(
                        instance = %instance.instance_id,
                        "certificate revoked, skipping registration for this instance"
                    );
                    return Ok(ResolveOutcome { addresses, attestations: None, unresolved: false });
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

        Ok(ResolveOutcome { addresses, attestations: Some(all_attestations), unresolved: false })
    }

    /// Runs one discovery cycle and resolves every instance into the
    /// [`DiscoveryResolution`] consumed by the spawn-and-reap loop.
    ///
    /// This fans out per-instance resolution work concurrently (bounded by
    /// [`SignerManagerConfig::max_concurrency`]). No registration transactions are
    /// submitted here; the [`Self::run`] loop spawns a dedicated task per
    /// registerable signer instead, so long Boundless proofs do not block the
    /// next discovery cycle.
    ///
    /// **Why no outer cancel-select.** `resolve_instance` performs several
    /// side effects before deciding whether an instance is registerable. The
    /// buffered stream is therefore drained to natural completion; each
    /// `resolve_instance` short-circuits on the configured cancellation token between
    /// awaits. Shutdown latency is bounded by `max_concurrency` × the slowest
    /// signer-RPC / CRL-fetch timeout, not by long proof work (which lives in
    /// the spawned proof tasks).
    ///
    /// The returned `ok_to_dereg` flag bakes in the cancellation policy
    /// (token not cancelled), the inconclusive-resolution guard (no
    /// `unresolved_instance_ids`), and the majority guard
    /// (`reachable * 2 > total`). The empty-discovery case sets it to
    /// `true` so legitimate fleet drains still let orphan cleanup proceed —
    /// except when the driver is cancelled, in which case the orphan dereg
    /// pass is skipped so we don't acquire nonces during shutdown.
    async fn discover_and_resolve(&self) -> Result<DiscoveryResolution> {
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
        let mut active_signers: HashSet<Address> = HashSet::new();
        let mut reachable_count = 0usize;
        let mut registerable: Vec<RegisterableSigner> = Vec::new();
        let mut unresolved_instance_ids: HashSet<String> = HashSet::new();

        let concurrency = self.config.signer_manager.max_concurrency.max(1);
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
        .buffer_unordered(concurrency);

        // No cancel-select around `futs.next()`: each future checks
        // cancellation cooperatively between awaits, so new work is
        // short-circuited while already-started resolution work reaches a
        // natural boundary.
        while let Some((instance, result)) = futs.next().await {
            match result {
                Ok(outcome) => {
                    reachable_count += 1;
                    for addr in &outcome.addresses {
                        active_signers.insert(*addr);
                    }
                    if outcome.unresolved {
                        unresolved_instance_ids.insert(instance.instance_id.clone());
                    }
                    if let Some(attestations) = outcome.attestations {
                        // `resolve_instance` already enforced the pairing
                        // invariants (non-empty addresses,
                        // `attestations.len() >= addresses.len()`) with
                        // richer per-instance errors. Flatten one entry
                        // per (signer, attestation) so the spawn pass in
                        // `reconcile_proof_tasks` becomes a flat
                        // iteration. The `zip` truncates at the shorter
                        // side, which mirrors the upstream invariant —
                        // any extra trailing attestations are dropped on
                        // the floor as before.
                        for (enclave_index, (signer, attestation)) in
                            outcome.addresses.into_iter().zip(attestations).enumerate()
                        {
                            registerable.push(RegisterableSigner {
                                instance: instance.clone(),
                                signer,
                                attestation,
                                enclave_index,
                            });
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        instance = %instance.instance_id,
                        endpoint = %instance.endpoint,
                        "failed to resolve instance"
                    );
                    RegistrarMetrics::processing_errors_total().increment(1);
                    // Mark this instance as inconclusive so reconcile
                    // does NOT cancel in-flight proof tasks tied to it
                    // (see `DiscoveryResolution::unresolved_instance_ids`
                    // for the rationale).
                    unresolved_instance_ids.insert(instance.instance_id.clone());
                }
            }
        }

        let ok_to_dereg =
            if self.config.cancel.is_cancelled() || !unresolved_instance_ids.is_empty() {
                false
            } else if total_count == 0 {
                true
            } else {
                // Plain `* 2` (rather than `saturating_mul`) — `reachable_count`
                // is bounded above by `total_count = instances.len()`, so the
                // doubling can only overflow on a list with `usize::MAX / 2`
                // entries, which is physically impossible.
                reachable_count * 2 > total_count
            };

        Ok(DiscoveryResolution {
            registerable,
            active_signers,
            reachable_count,
            total_count,
            ok_to_dereg,
            unresolved_instance_ids,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::SystemTime,
    };

    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{Address, B256, Bloom, Bytes};
    use alloy_rpc_types_eth::TransactionReceipt;
    use alloy_sol_types::SolCall as _;
    use async_trait::async_trait;
    use base_proof_contracts::ITEEProverRegistry;
    use base_proof_tee_nitro_attestation_prover::{AttestationProof, AttestationProofProvider};
    use base_tx_manager::{SendHandle, TxCandidate};
    use rstest::rstest;
    use tokio_util::sync::CancellationToken;
    use url::Url;

    use super::*;
    use crate::{
        InstanceHealthStatus, RegistryClient, Result, SignerClient, SignerManager,
        test_utils::{
            EP1, EP2, EP3, EP4, HARDHAT_KEY_0, HARDHAT_KEY_1, HARDHAT_KEY_2, HARDHAT_KEY_3,
            NoopNitroVerifier, NoopTxManager, TEST_REGISTRY_ADDRESS, healthy_prover_instance,
            prover_instance, public_key_from_private, signer_from_private_key,
        },
    };

    const ALL_ENDPOINTS: [&str; 4] = [EP1, EP2, EP3, EP4];
    const ALL_KEYS: [&[u8; 32]; 4] =
        [&HARDHAT_KEY_0, &HARDHAT_KEY_1, &HARDHAT_KEY_2, &HARDHAT_KEY_3];
    const GATED_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

    #[derive(Debug)]
    struct MockDiscovery {
        instances: Vec<ProverInstance>,
    }

    #[async_trait]
    impl InstanceDiscovery for MockDiscovery {
        async fn discover_instances(&self) -> Result<Vec<ProverInstance>> {
            Ok(self.instances.clone())
        }
    }

    #[derive(Debug, Clone)]
    struct MutableDiscovery {
        instances: Arc<Mutex<Vec<ProverInstance>>>,
    }

    impl MutableDiscovery {
        fn new(initial: Vec<ProverInstance>) -> Self {
            Self { instances: Arc::new(Mutex::new(initial)) }
        }

        fn set(&self, instances: Vec<ProverInstance>) {
            *self.instances.lock().unwrap() = instances;
        }
    }

    #[async_trait]
    impl InstanceDiscovery for MutableDiscovery {
        async fn discover_instances(&self) -> Result<Vec<ProverInstance>> {
            Ok(self.instances.lock().unwrap().clone())
        }
    }

    #[derive(Debug)]
    struct MockSignerClient {
        keys: HashMap<Url, Vec<Vec<u8>>>,
        attestations: HashMap<Url, Vec<Vec<u8>>>,
        fail_attestation: HashSet<Url>,
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
            Self { keys, attestations: HashMap::new(), fail_attestation: HashSet::new() }
        }

        fn multi_enclave(host_port: &str, private_keys: &[&[u8; 32]]) -> Self {
            let pubs = private_keys.iter().map(|pk| public_key_from_private(pk)).collect();
            Self {
                keys: HashMap::from([(endpoint_url(host_port), pubs)]),
                attestations: HashMap::new(),
                fail_attestation: HashSet::new(),
            }
        }

        fn with_attestations(mut self, host_port: &str, attestations: Vec<Vec<u8>>) -> Self {
            self.attestations.insert(endpoint_url(host_port), attestations);
            self
        }

        fn with_attestation_failure(mut self, host_port: &str) -> Self {
            self.fail_attestation.insert(endpoint_url(host_port));
            self
        }
    }

    #[async_trait]
    impl SignerClient for MockSignerClient {
        async fn signer_public_key(&self, endpoint: &Url) -> Result<Vec<Vec<u8>>> {
            self.keys.get(endpoint).cloned().ok_or_else(|| RegistrarError::ProverClient {
                instance: endpoint.to_string(),
                source: "unreachable".into(),
            })
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

    fn endpoint_url(host_port: &str) -> Url {
        Url::parse(&format!("http://{host_port}")).unwrap()
    }

    fn stub_receipt() -> TransactionReceipt {
        let inner = ReceiptEnvelope::Legacy(ReceiptWithBloom {
            receipt: Receipt {
                status: Eip658Value::Eip658(true),
                cumulative_gas_used: 21_000,
                logs: vec![],
            },
            logs_bloom: Bloom::ZERO,
        });
        TransactionReceipt {
            inner,
            transaction_hash: B256::ZERO,
            transaction_index: Some(0),
            block_hash: Some(B256::ZERO),
            block_number: Some(1),
            gas_used: 21_000,
            effective_gas_price: 1_000_000_000,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: Some(Address::ZERO),
            contract_address: None,
        }
    }

    fn mock_cert_manager<T: TxManager + 'static>(
        config: &DriverConfig,
        tx_manager: T,
    ) -> CertManager<T> {
        CertManager::new(&config.crl, Arc::new(NoopNitroVerifier), tx_manager)
            .expect("test cert manager builds")
    }

    #[derive(Debug, Clone)]
    struct MockRegistry;

    #[async_trait]
    impl RegistryClient for MockRegistry {
        async fn is_registered(&self, _signer: Address) -> Result<bool> {
            Ok(false)
        }

        async fn get_registered_signers(&self) -> Result<Vec<Address>> {
            Ok(vec![])
        }
    }

    #[derive(Debug, Clone)]
    struct SharedTxManager {
        sent: Arc<Mutex<Vec<Bytes>>>,
    }

    impl SharedTxManager {
        fn new() -> Self {
            Self { sent: Arc::new(Mutex::new(vec![])) }
        }

        fn sent_calldata(&self) -> Vec<Bytes> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl TxManager for SharedTxManager {
        async fn send(&self, candidate: TxCandidate) -> base_tx_manager::SendResponse {
            self.sent.lock().unwrap().push(candidate.tx_data);
            Ok(stub_receipt())
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            unreachable!("driver tests do not submit async transactions")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    #[derive(Debug)]
    struct GatedProofState {
        release: CancellationToken,
        call_count: AtomicUsize,
        in_flight: AtomicUsize,
    }

    impl Default for GatedProofState {
        fn default() -> Self {
            Self {
                release: CancellationToken::new(),
                call_count: AtomicUsize::new(0),
                in_flight: AtomicUsize::new(0),
            }
        }
    }

    struct InFlightGuard {
        state: Arc<GatedProofState>,
    }

    impl InFlightGuard {
        fn new(state: Arc<GatedProofState>) -> Self {
            state.in_flight.fetch_add(1, Ordering::SeqCst);
            Self { state }
        }
    }

    impl Drop for InFlightGuard {
        fn drop(&mut self) {
            self.state.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[derive(Debug, Clone)]
    struct GatedProofProvider {
        state: Arc<GatedProofState>,
    }

    impl GatedProofProvider {
        fn new() -> (Self, GatedProofHandles) {
            let state = Arc::new(GatedProofState::default());
            (Self { state: Arc::clone(&state) }, GatedProofHandles { state })
        }
    }

    #[async_trait]
    impl AttestationProofProvider for GatedProofProvider {
        async fn generate_proof(
            &self,
            _attestation_bytes: &[u8],
            cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            self.state.call_count.fetch_add(1, Ordering::SeqCst);
            let _guard = InFlightGuard::new(Arc::clone(&self.state));

            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    Err(base_proof_tee_nitro_attestation_prover::ProverError::Boundless(
                        "proof cancelled by test harness".into(),
                    ))
                }
                () = self.state.release.cancelled() => {
                    Ok(AttestationProof {
                        output: Bytes::new(),
                        proof_bytes: Bytes::from_static(b"gated-proof"),
                    })
                }
            };
            result
        }

        async fn generate_proof_for_signer(
            &self,
            attestation_bytes: &[u8],
            signer_address: Address,
            cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            let mut proof = self.generate_proof(attestation_bytes, cancel).await?;
            proof.output = Bytes::copy_from_slice(signer_address.as_slice());
            Ok(proof)
        }
    }

    #[derive(Debug, Clone)]
    struct GatedProofHandles {
        state: Arc<GatedProofState>,
    }

    impl GatedProofHandles {
        fn release_all(&self) {
            self.state.release.cancel();
        }

        fn call_count(&self) -> usize {
            self.state.call_count.load(Ordering::SeqCst)
        }

        fn in_flight(&self) -> usize {
            self.state.in_flight.load(Ordering::SeqCst)
        }
    }

    type RunDriver = RegistrationDriver<
        MutableDiscovery,
        MockSignerClient,
        Arc<SignerManager<GatedProofProvider, MockRegistry, SharedTxManager>>,
        SharedTxManager,
    >;

    #[derive(Debug)]
    struct GatedRunHarness {
        driver: Arc<RunDriver>,
        cancel: CancellationToken,
        discovery: MutableDiscovery,
        proof: GatedProofHandles,
        tx: SharedTxManager,
    }

    impl GatedRunHarness {
        fn new(
            initial_instances: Vec<ProverInstance>,
            endpoints_to_keys: &[(&str, &[u8; 32])],
        ) -> Self {
            let discovery = MutableDiscovery::new(initial_instances);
            let signer_client = MockSignerClient::from_keys(endpoints_to_keys);
            let cancel = CancellationToken::new();
            let mut config = default_config(cancel.clone());
            config.poll_interval = FAST_POLL_INTERVAL;
            let registry = MockRegistry;
            let tx = SharedTxManager::new();
            let (proof_provider, proof_handles) = GatedProofProvider::new();
            let signer_manager = Arc::new(SignerManager::new(
                proof_provider,
                registry,
                tx.clone(),
                config.signer_manager.clone(),
            ));
            let cert_manager = mock_cert_manager(&config, tx.clone());
            let driver = Arc::new(RegistrationDriver::new(
                discovery.clone(),
                signer_client,
                config,
                cert_manager,
                signer_manager,
            ));

            Self { driver, cancel, discovery, proof: proof_handles, tx }
        }

        fn spawn_run(&self) -> tokio::task::JoinHandle<Result<()>> {
            let driver = Arc::clone(&self.driver);
            tokio::spawn(async move { driver.run_loop().await })
        }

        async fn shutdown(&self, handle: tokio::task::JoinHandle<Result<()>>) {
            self.cancel.cancel();
            let outcome = tokio::time::timeout(GATED_WAIT_TIMEOUT, handle)
                .await
                .expect("run loop should observe cancellation")
                .expect("run loop task should not panic");
            outcome.expect("run loop should stop cleanly");
        }
    }

    async fn wait_for(label: &str, predicate: impl Fn() -> bool) {
        let started = std::time::Instant::now();
        while !predicate() {
            if started.elapsed() > GATED_WAIT_TIMEOUT {
                panic!("timed out waiting for: {label}");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn count_register_calls(sent: &[Bytes]) -> usize {
        let sel = ITEEProverRegistry::registerSignerCall::SELECTOR;
        sent.iter().filter(|c| c.starts_with(&sel)).count()
    }

    #[derive(Debug, Clone)]
    struct MockSignerManager {
        reconcile_calls: Arc<AtomicUsize>,
        orphan_calls: Arc<Mutex<Vec<HashSet<Address>>>>,
        cancel_after_orphan: CancellationToken,
    }

    impl MockSignerManager {
        fn new(cancel_after_orphan: CancellationToken) -> Self {
            Self {
                reconcile_calls: Arc::new(AtomicUsize::new(0)),
                orphan_calls: Arc::new(Mutex::new(Vec::new())),
                cancel_after_orphan,
            }
        }

        fn reconcile_count(&self) -> usize {
            self.reconcile_calls.load(Ordering::SeqCst)
        }

        fn orphan_inputs(&self) -> Vec<HashSet<Address>> {
            self.orphan_calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SignerLifecycle for MockSignerManager {
        fn reconcile_proof_tasks(
            &self,
            _resolution: &DiscoveryResolution,
            _proof_tasks: &mut ProofTaskSet,
            _cancel: &CancellationToken,
        ) {
            self.reconcile_calls.fetch_add(1, Ordering::SeqCst);
        }

        async fn run_orphan_dereg(
            &self,
            protected_signers: &HashSet<Address>,
            _cancel: &CancellationToken,
        ) -> Result<()> {
            self.orphan_calls.lock().unwrap().push(protected_signers.clone());
            self.cancel_after_orphan.cancel();
            Ok(())
        }
    }

    fn default_config(cancel: CancellationToken) -> DriverConfig {
        DriverConfig {
            poll_interval: Duration::from_secs(1),
            cancel,
            signer_manager: SignerManagerConfig {
                registry_address: TEST_REGISTRY_ADDRESS,
                max_concurrency: DEFAULT_MAX_CONCURRENCY,
                max_tx_retries: DEFAULT_MAX_TX_RETRIES,
                tx_retry_delay: Duration::from_secs(DEFAULT_TX_RETRY_DELAY_SECS),
            },
            unhealthy_registration_window: Duration::from_secs(
                DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS,
            ),
            crl: CrlConfig {
                enabled: false,
                nitro_verifier_address: None,
                fetch_timeout: Duration::from_secs(crate::DEFAULT_CRL_FETCH_TIMEOUT_SECS),
            },
        }
    }

    fn cycle_driver<S>(
        instances: Vec<ProverInstance>,
        signer_client: S,
        cancel: CancellationToken,
    ) -> RegistrationDriver<MockDiscovery, S, MockSignerManager, NoopTxManager>
    where
        S: SignerClient + 'static,
    {
        let config = default_config(cancel);
        let signer_manager = MockSignerManager::new(CancellationToken::new());
        let cert_manager = mock_cert_manager(&config, NoopTxManager);
        RegistrationDriver::new(
            MockDiscovery { instances },
            signer_client,
            config,
            cert_manager,
            signer_manager,
        )
    }

    const FAST_POLL_INTERVAL: Duration = Duration::from_millis(25);

    #[tokio::test]
    async fn run_loop_uses_injected_signer_manager_boundary() {
        let cancel = CancellationToken::new();
        let mut config = default_config(cancel.clone());
        config.poll_interval = FAST_POLL_INTERVAL;

        let signer_manager = MockSignerManager::new(cancel.clone());
        let signer_manager_handle = signer_manager.clone();
        let signer = signer_from_private_key(&HARDHAT_KEY_0);
        let cert_manager = mock_cert_manager(&config, NoopTxManager);

        let driver = RegistrationDriver::new(
            MockDiscovery { instances: vec![healthy_prover_instance(EP1)] },
            MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]),
            config,
            cert_manager,
            signer_manager,
        );

        driver.run_loop().await.expect("driver exits after mock orphan pass cancels");

        assert_eq!(
            signer_manager_handle.reconcile_count(),
            1,
            "driver should delegate registerable reconciliation to signer manager",
        );
        let orphan_inputs = signer_manager_handle.orphan_inputs();
        assert_eq!(orphan_inputs.len(), 1, "driver should delegate one orphan pass");
        assert!(
            orphan_inputs[0].contains(&signer),
            "driver passes active signer set into orphan protection",
        );
    }

    #[rstest]
    #[case::one_instance(1)]
    #[case::two_instances(2)]
    #[tokio::test]
    async fn run_loop_registers_each_discovered_enclave(#[case] num_instances: usize) {
        let keys: Vec<(&str, &[u8; 32])> =
            ALL_ENDPOINTS.iter().copied().zip(ALL_KEYS.iter().copied()).collect();
        let instances: Vec<_> =
            ALL_ENDPOINTS[..num_instances].iter().map(|ep| healthy_prover_instance(ep)).collect();
        let harness = GatedRunHarness::new(instances, &keys);

        let run_handle = harness.spawn_run();

        wait_for("every proof task parked in gate", || harness.proof.in_flight() == num_instances)
            .await;
        harness.proof.release_all();
        wait_for("every registerSigner transaction submitted", || {
            count_register_calls(&harness.tx.sent_calldata()) == num_instances
        })
        .await;
        harness.shutdown(run_handle).await;

        assert_eq!(harness.proof.call_count(), num_instances, "exactly one proof per enclave");
    }

    #[tokio::test]
    async fn run_loop_continues_discovery_while_proofs_are_in_flight() {
        let keys: Vec<(&str, &[u8; 32])> =
            ALL_ENDPOINTS.iter().copied().zip(ALL_KEYS.iter().copied()).collect();
        let harness = GatedRunHarness::new(vec![healthy_prover_instance(EP1)], &keys);

        let run_handle = harness.spawn_run();

        wait_for("initial proof task parked in gate", || harness.proof.in_flight() == 1).await;
        harness.discovery.set(vec![healthy_prover_instance(EP1), healthy_prover_instance(EP2)]);
        wait_for("newly discovered proof task parked while first proof is still in flight", || {
            harness.proof.in_flight() == 2
        })
        .await;

        harness.proof.release_all();
        wait_for("both registerSigner transactions submitted", || {
            count_register_calls(&harness.tx.sent_calldata()) == 2
        })
        .await;
        harness.shutdown(run_handle).await;

        assert_eq!(harness.proof.call_count(), 2, "new signer must not wait for old proof");
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

        let resolution = driver.discover_and_resolve().await.unwrap();
        assert_eq!(
            resolution.registerable.len(),
            1,
            "recently-launched unhealthy instance should be registerable"
        );
        assert_eq!(resolution.registerable[0].signer, addr);
        assert!(
            resolution.active_signers.contains(&addr),
            "recently-launched unhealthy signer should be in active_signers"
        );
        assert!(resolution.ok_to_dereg, "resolved instance permits orphan cleanup");
    }

    #[tokio::test]
    async fn discover_and_resolve_allows_orphan_pass_when_discovery_is_empty() {
        let driver =
            cycle_driver(vec![], MockSignerClient::from_keys(&[]), CancellationToken::new());

        let resolution = driver.discover_and_resolve().await.unwrap();
        assert!(resolution.active_signers.is_empty(), "no instances → no active signers");
        assert!(
            resolution.ok_to_dereg,
            "zero-instance fleet drain is a legitimate empty active set",
        );
    }

    #[tokio::test]
    async fn discover_and_resolve_clears_ok_to_dereg_when_cancelled_before_run() {
        let instances = vec![healthy_prover_instance(EP1), healthy_prover_instance(EP2)];

        let signer_client =
            MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)]);

        let cancel = CancellationToken::new();
        let driver = cycle_driver(instances, signer_client, cancel.clone());

        cancel.cancel();
        let resolution = driver.discover_and_resolve().await.unwrap();

        assert!(!resolution.ok_to_dereg, "cancellation must clear ok_to_dereg",);
    }

    #[derive(Debug)]
    struct CancellingSignerClient {
        inner: MockSignerClient,
        cancel: CancellationToken,
    }

    #[async_trait]
    impl SignerClient for CancellingSignerClient {
        async fn signer_public_key(&self, endpoint: &Url) -> Result<Vec<Vec<u8>>> {
            let result = self.inner.signer_public_key(endpoint).await;
            if result.is_ok() {
                self.cancel.cancel();
            }
            result
        }

        async fn signer_attestation(
            &self,
            endpoint: &Url,
            user_data: Option<Vec<u8>>,
            nonce: Option<Vec<u8>>,
        ) -> Result<Vec<Vec<u8>>> {
            self.inner.signer_attestation(endpoint, user_data, nonce).await
        }
    }

    #[tokio::test]
    async fn discover_and_resolve_clears_ok_to_dereg_when_cancelled_mid_resolution() {
        let cancel = CancellationToken::new();
        let signer_client = CancellingSignerClient {
            inner: MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]),
            cancel: cancel.clone(),
        };
        let driver = cycle_driver(vec![healthy_prover_instance(EP1)], signer_client, cancel);

        let resolution = driver.discover_and_resolve().await.unwrap();

        assert!(
            !resolution.ok_to_dereg,
            "cancellation observed during resolution must clear ok_to_dereg",
        );
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

        let resolution = driver.discover_and_resolve().await.unwrap();

        assert_eq!(resolution.reachable_count, 1);
        assert_eq!(resolution.total_count, 3);
        assert!(!resolution.ok_to_dereg, "1/3 reachable must block orphan-deregistration",);
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

        let resolution = driver.discover_and_resolve().await.unwrap();
        assert_eq!(
            resolution.registerable.len(),
            reachable.len(),
            "all reachable instances should be registerable despite 1 unreachable",
        );
        assert!(
            resolution.unresolved_instance_ids.contains(&unreachable.instance_id),
            "unreachable instance must be marked as unresolved so reconcile skips its cancel-pass",
        );
        assert!(!resolution.ok_to_dereg, "unresolved instance must block orphan-dereg",);
    }

    #[tokio::test]
    async fn discover_and_resolve_multi_enclave_draining_protects_all_signers_from_deregistration()
    {
        let addr0 = signer_from_private_key(&HARDHAT_KEY_0);
        let addr1 = signer_from_private_key(&HARDHAT_KEY_1);

        let instances = vec![prover_instance(EP1, InstanceHealthStatus::Draining, None)];
        let signer_client = MockSignerClient::multi_enclave(EP1, &[&HARDHAT_KEY_0, &HARDHAT_KEY_1]);

        let driver = cycle_driver(instances, signer_client, CancellationToken::new());

        let resolution = driver.discover_and_resolve().await.unwrap();
        assert!(
            resolution.registerable.is_empty(),
            "draining instance must not appear in the registerable set",
        );
        assert!(resolution.active_signers.contains(&addr0));
        assert!(resolution.active_signers.contains(&addr1));
        assert!(resolution.ok_to_dereg);
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

        let resolution = driver.discover_and_resolve().await.unwrap();
        assert_eq!(
            resolution.registerable.len(),
            1,
            "only the healthy instance should be registerable",
        );
        assert_eq!(resolution.registerable[0].signer, addr_healthy);
        assert!(
            resolution.active_signers.contains(&addr_unhealthy),
            "unhealthy signer must remain in active_signers to block dereg",
        );
        assert!(resolution.ok_to_dereg);
    }

    #[rstest]
    #[case::mismatch(false)]
    #[case::rpc_error(true)]
    #[tokio::test]
    async fn discover_and_resolve_attestation_failure_keeps_signer_active_and_unresolved(
        #[case] rpc_error: bool,
    ) {
        let signer_addr = signer_from_private_key(&HARDHAT_KEY_0);
        let inst = healthy_prover_instance(EP1);
        let signer_client = if rpc_error {
            MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]).with_attestation_failure(EP1)
        } else {
            MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]).with_attestations(EP1, vec![])
        };

        let driver = cycle_driver(vec![inst.clone()], signer_client, CancellationToken::new());

        let resolution = driver.discover_and_resolve().await.unwrap();

        assert!(
            resolution.active_signers.contains(&signer_addr),
            "partial resolution must keep the signer in the active set",
        );
        assert!(
            resolution.registerable.is_empty(),
            "without a matching attestation the signer must not be registerable",
        );
        assert!(
            resolution.unresolved_instance_ids.contains(&inst.instance_id),
            "partial resolution must preserve in-flight tasks for the instance",
        );
        assert!(!resolution.ok_to_dereg, "unresolved attestation state must block orphan-dereg",);
    }

    #[derive(Debug)]
    struct ConcurrencyTrackingSignerClient {
        inner: MockSignerClient,
        in_flight: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    impl ConcurrencyTrackingSignerClient {
        fn new(inner: MockSignerClient) -> (Self, Arc<AtomicUsize>) {
            let peak = Arc::new(AtomicUsize::new(0));
            let client =
                Self { inner, in_flight: Arc::new(AtomicUsize::new(0)), peak: Arc::clone(&peak) };
            (client, peak)
        }
    }

    #[async_trait]
    impl SignerClient for ConcurrencyTrackingSignerClient {
        async fn signer_public_key(&self, endpoint: &Url) -> Result<Vec<Vec<u8>>> {
            let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(current, Ordering::SeqCst);

            tokio::task::yield_now().await;

            let result = self.inner.signer_public_key(endpoint).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            result
        }

        async fn signer_attestation(
            &self,
            endpoint: &Url,
            user_data: Option<Vec<u8>>,
            nonce: Option<Vec<u8>>,
        ) -> Result<Vec<Vec<u8>>> {
            self.inner.signer_attestation(endpoint, user_data, nonce).await
        }
    }

    #[tokio::test]
    async fn discover_and_resolve_respects_max_concurrency() {
        let max_concurrency = 2;
        let endpoints = [EP1, EP2, EP3, EP4];
        let private_keys = [&HARDHAT_KEY_0, &HARDHAT_KEY_1, &HARDHAT_KEY_2, &HARDHAT_KEY_3];

        let keys: Vec<(&str, &[u8; 32])> =
            endpoints.iter().copied().zip(private_keys.iter().copied()).collect();
        let instances: Vec<_> = endpoints.iter().map(|ep| healthy_prover_instance(ep)).collect();
        let inner = MockSignerClient::from_keys(&keys);
        let (signer_client, peak) = ConcurrencyTrackingSignerClient::new(inner);

        let cancel = CancellationToken::new();
        let mut config = default_config(cancel);
        config.signer_manager.max_concurrency = max_concurrency;
        let signer_manager = MockSignerManager::new(CancellationToken::new());
        let cert_manager = mock_cert_manager(&config, NoopTxManager);

        let driver = RegistrationDriver::new(
            MockDiscovery { instances },
            signer_client,
            config,
            cert_manager,
            signer_manager,
        );

        let resolution = driver.discover_and_resolve().await.unwrap();

        let observed_peak = peak.load(Ordering::SeqCst);
        assert!(
            observed_peak <= max_concurrency,
            "peak concurrency {observed_peak} exceeded max_concurrency {max_concurrency}",
        );
        assert_eq!(
            resolution.registerable.len(),
            endpoints.len(),
            "all 4 healthy instances should resolve into the registerable set",
        );
    }
}
