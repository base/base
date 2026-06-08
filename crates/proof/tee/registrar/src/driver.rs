//! Registration driver — core orchestration loop.
//!
//! Discovers prover instances, checks onchain registration status, generates
//! ZK proofs for unregistered signers, and submits registration transactions
//! to L1 via the [`TxManager`]. Also detects orphaned onchain signers (those
//! no longer backed by a healthy instance) and deregisters them.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::{Address, Bytes, hex};
use alloy_sol_types::SolCall;
use base_proof_contracts::ITEEProverRegistry;
use base_proof_tee_nitro_attestation_prover::AttestationProofProvider;
use base_tx_manager::{TxCandidate, TxManager};
use futures::stream::StreamExt;
use rand::random;
use tokio::{
    sync::Semaphore,
    task::{self, JoinSet},
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::{
    CertManager, CrlConfig, InstanceDiscovery, InstanceHealthStatus, NitroVerifierClient,
    ProofHandlerConfig, ProverClient, ProverInstance, RegistrarError, RegistrarMetrics,
    RegistrationManager, RegistryClient, Result, SignerClient,
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
    /// `TEEProverRegistry` contract address on L1.
    pub registry_address: Address,
    /// Interval between discovery and registration poll cycles.
    pub poll_interval: Duration,
    /// Cancellation token for graceful shutdown.
    pub cancel: CancellationToken,
    /// Maximum number of instances to process concurrently. Each instance
    /// may trigger proof generation, so this bounds concurrent proof work.
    /// Defaults to [`DEFAULT_MAX_CONCURRENCY`].
    pub max_concurrency: usize,
    /// Maximum number of transaction submission retries for transient errors.
    /// Defaults to [`DEFAULT_MAX_TX_RETRIES`].
    pub max_tx_retries: u32,
    /// Delay between transaction submission retries.
    /// Defaults to [`DEFAULT_TX_RETRY_DELAY_SECS`] seconds.
    pub tx_retry_delay: Duration,
    /// Duration after launch during which unhealthy instances are still
    /// eligible for registration. New instances may fail ALB health checks
    /// while the application is still initializing. Set to zero to disable.
    /// Defaults to [`DEFAULT_UNHEALTHY_REGISTRATION_WINDOW_SECS`] seconds.
    pub unhealthy_registration_window: Duration,
    /// CRL checking configuration. When enabled, intermediate certificates
    /// are checked against CRL distribution points before registration.
    pub crl: CrlConfig,
}

/// State for a proof-generation task currently in-flight in the
/// [`RegistrationDriver::run`] spawn-and-reap loop.
///
/// One entry per signer address — the `pending` map is keyed by
/// [`Address`] (the signer this task owns) so reconcile's per-cycle
/// match against the latest registerable set is an O(1) lookup and
/// "at most one active proof task per signer" is a structural
/// invariant of the map, not a runtime check.
///
/// `cancel` is a child of [`DriverConfig::cancel`]. Firing it asks the
/// task to terminate cooperatively at the next checkpoint (proof-gen,
/// retry sleep, before tx send) — the task always returns the signer
/// address (`Ok(signer)`) when it observes the cancel, never an error,
/// so the happy and cancelled paths route identically through
/// [`RegistrationDriver::apply_join_outcome`].
#[derive(Debug)]
pub struct PendingRegistration {
    /// Originating instance ID — recorded only for logging.
    pub instance_id: String,
    /// `JoinSet` task id for this proof task. Used by
    /// [`RegistrationDriver::apply_join_outcome`] for two things: (1)
    /// recovering the signer address on failure paths via an O(n) scan
    /// over `pending` (bounded by [`DriverConfig::max_concurrency`]),
    /// and (2) gating success-arm cleanup with a `task_id == id` check
    /// so a stale task's terminal outcome cannot evict a same-signer
    /// respawn that reconcile dropped into the slot mid-cycle.
    pub task_id: task::Id,
    /// Cooperative cancel handle for this single task.
    pub cancel: CancellationToken,
    /// `true` once [`RegistrationDriver::reconcile_proof_tasks`] has
    /// fired this task's [`Self::cancel`] (signer dropped from the
    /// registerable set mid-flight). Lets [`RegistrationDriver::
    /// drain_proof_tasks`] distinguish reconcile-cancelled tasks
    /// (already counted in `proof_tasks_cancelled` at intent time)
    /// from shutdown-cancelled tasks so neither double-counts nor
    /// silently misses the shutdown path — every cancellation increments
    /// the metric exactly once. Necessary because [`Self::cancel`] is a
    /// child of [`DriverConfig::cancel`]; on shutdown the parent's fire
    /// auto-cancels every child, so an `is_cancelled()` gate alone
    /// cannot tell the two cases apart.
    pub cancelled_by_reconcile: bool,
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

/// Core registration loop tying together discovery, attestation polling,
/// ZK proof generation, and onchain submission.
///
/// Generic over the discovery, proof generation, registry, transaction
/// manager, and signer client backends so each can be mocked independently
/// in tests.
pub struct RegistrationDriver<D, P, R, T, S> {
    discovery: D,
    proof_provider: P,
    registry: R,
    tx_manager: T,
    signer_client: S,
    config: DriverConfig,
    /// Optional certificate revocation manager. Built once at construction
    /// time when CRL checking is enabled. `None` when CRL checking is disabled.
    cert_manager: Option<CertManager>,
    /// Bounds the number of proof-generation calls that may be in-flight
    /// across the spawned task pool at once. Sized from
    /// [`DriverConfig::max_concurrency`], matching the discovery/resolve
    /// concurrency bound so an ASG scale-up cannot fan out an unbounded
    /// number of concurrent Boundless proof requests. Permits are acquired
    /// inside [`Self::run_proof_task`], not at spawn time, so the
    /// reconcile pass remains synchronous.
    proof_semaphore: Arc<Semaphore>,
    /// Process-local set of signer addresses currently being registered.
    ///
    /// [`RegistrationManager::register_signer`] reserves an entry here before
    /// its `is_registered` precheck and releases it when it returns. This closes
    /// a TOCTOU race in which two concurrent registration attempts for the same
    /// signer both read `is_registered == false`, both generate proofs, and both
    /// submit duplicate registration transactions.
    ///
    /// This is a defence-in-depth backstop: the [`Self::run`] spawn loop's
    /// `in_flight: HashSet<Address>` already dedupes at task-spawn time,
    /// so the registration-manager layer only catches duplicates from callers
    /// that bypass the spawn loop.
    ///
    /// The set is held across the entire registration lifecycle (including
    /// the ~20 minute Boundless proof generation) so deduplication holds
    /// across cycles as well as within one.
    in_flight_registrations: Arc<Mutex<HashSet<Address>>>,
    /// Last-known EC2 instance ID for every signer address the registrar
    /// has ever observed advertising itself in a discovery cycle.
    /// Updated in `discover_and_resolve`, consulted by
    /// `submit_deregistration` so the "Deregistering signer" log line
    /// can attribute the orphan to the EC2 instance it last lived on.
    /// `None` on dereg is the strongest single diagnostic that another
    /// registrar (or a prior deployment) wrote the signer.
    /// Entries are never evicted — bounded by historic fleet size.
    signer_history: Arc<Mutex<HashMap<Address, String>>>,
}

impl<D, P, R, T, S> fmt::Debug for RegistrationDriver<D, P, R, T, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistrationDriver").field("config", &self.config).finish_non_exhaustive()
    }
}

impl<D, P, R, T, S> RegistrationDriver<D, P, R, T, S>
where
    D: InstanceDiscovery + 'static,
    P: AttestationProofProvider + 'static,
    R: RegistryClient + 'static,
    T: TxManager + 'static,
    S: SignerClient + 'static,
{
    /// Creates a new registration driver.
    ///
    /// When CRL checking is enabled, pre-builds the HTTP client used for
    /// CRL fetches so it can be reused across registration cycles. The
    /// optional `nitro_verifier` client consults the onchain durable
    /// revocation sentinel before each registration; pass `None` to disable
    /// the onchain pre-check (useful for tests and unit deployments).
    ///
    /// # Errors
    ///
    /// Returns [`RegistrarError::Config`] when `config.crl.enabled` is `true`
    /// and either the `nitro_verifier` client is missing or the
    /// [`CertManager`] fails to initialize. Failing fast prevents a
    /// misconfigured driver from silently bypassing CRL protection at runtime.
    pub fn new(
        discovery: D,
        proof_provider: P,
        registry: R,
        tx_manager: T,
        signer_client: S,
        config: DriverConfig,
        nitro_verifier: Option<Arc<dyn NitroVerifierClient>>,
    ) -> Result<Self> {
        let cert_manager = if config.crl.enabled {
            let Some(nitro_verifier) = nitro_verifier else {
                return Err(RegistrarError::Config(
                    "CRL checking enabled but nitro_verifier client not configured; \
                     a NitroEnclaveVerifier client is required as both the revokeCert \
                     destination and the onchain revokedCerts sentinel source"
                        .into(),
                ));
            };
            Some(CertManager::new(&config.crl, nitro_verifier)?)
        } else {
            None
        };
        let proof_semaphore = Arc::new(Semaphore::new(config.max_concurrency.max(1)));
        Ok(Self {
            discovery,
            proof_provider,
            registry,
            tx_manager,
            signer_client,
            config,
            cert_manager,
            proof_semaphore,
            in_flight_registrations: Arc::new(Mutex::new(HashSet::new())),
            signer_history: Arc::new(Mutex::new(HashMap::new())),
        })
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
    ///    `pending` (see [`Self::protected_signers`]) so a signer
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
    /// `apply_join_outcome`, keeping the proof-task metrics consistent.
    /// `JoinSet::abort_all` is deliberately **not** used — see
    /// [`Self::drain_proof_tasks`] for the nonce-gap rationale.
    ///
    /// # Ownership
    ///
    /// Consumes `self` by value so the API matches every other long-lived
    /// `*_service::run` in the workspace. Internally the driver is wrapped
    /// in an [`Arc`] and the spawned proof tasks each hold a clone — see
    /// [`Self::run_arc`] for the underlying loop and the rationale for the
    /// shared ownership.
    pub async fn run(self) -> Result<()> {
        Arc::new(self).run_arc().await
    }

    /// Underlying registration loop that powers [`Self::run`].
    ///
    /// Takes `self: Arc<Self>` directly because each spawned proof task
    /// owns an `Arc<Self>` clone so the cycle loop can continue to mutate
    /// `pending` and `tasks` while proofs run for tens of minutes. Tests
    /// that need to inspect driver state from outside the task can call
    /// this method directly with their own `Arc` clone; production code
    /// uses [`Self::run`].
    pub async fn run_arc(self: Arc<Self>) -> Result<()> {
        info!(
            poll_interval = ?self.config.poll_interval,
            registry = %self.config.registry_address,
            "starting registration driver"
        );

        let mut tasks: JoinSet<Result<Address>> = JoinSet::new();
        let mut pending: HashMap<Address, PendingRegistration> = HashMap::new();

        loop {
            // Reap before discovery so finished tasks don't linger in
            // `pending` for an entire cycle and (incorrectly) cause
            // reconcile to skip spawning a replacement on transient
            // failure (audit finding #9).
            Self::reap_finished_tasks(&mut tasks, &mut pending);

            match self.discover_and_resolve().await {
                Ok(resolution) => {
                    // Reap again: a task that finished during the
                    // (potentially slow) discovery RPCs would otherwise
                    // look in-flight to reconcile and get spuriously
                    // re-cancelled or have its respawn deferred a cycle.
                    Self::reap_finished_tasks(&mut tasks, &mut pending);

                    // Spawning new proof tasks during a shutdown would
                    // acquire L1 nonces we have no intention of
                    // broadcasting. Skip reconcile (and the orphan
                    // dereg pass) entirely when cancellation is set.
                    if !self.config.cancel.is_cancelled() {
                        self.reconcile_proof_tasks(&resolution, &mut tasks, &mut pending);
                    }

                    if resolution.ok_to_dereg && !self.config.cancel.is_cancelled() {
                        // Protect every in-flight signer in addition to
                        // `active_signers`. An instance whose
                        // `resolve_instance` failed this cycle is in
                        // `unresolved_instance_ids` (so reconcile
                        // preserves its task), but its signer is absent
                        // from `active_signers`. Without this union the
                        // preserved task could complete and `register`
                        // the signer mid-pass, and the very same orphan
                        // sweep would then deregister it (TOCTOU).
                        let protected = Self::protected_signers(&resolution, &pending);
                        if let Err(e) = self.run_orphan_dereg(&protected).await {
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
            RegistrarMetrics::proof_tasks_pending().set(pending.len() as f64);

            tokio::select! {
                biased;
                () = self.config.cancel.cancelled() => {
                    info!(
                        pending = pending.len(),
                        "registration driver received shutdown signal"
                    );
                    break;
                }
                () = tokio::time::sleep(self.config.poll_interval) => {}
            }
        }

        Self::drain_proof_tasks(&mut tasks, &mut pending).await;

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
            // `run_arc` loop re-checks `cancel.is_cancelled()` before
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
            let cert_manager =
                self.cert_manager.as_ref().expect("cert_manager required when CRL enabled");
            match cert_manager
                .check_and_revoke_crls(first_attestation, instance, &self.tx_manager)
                .await
            {
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
    /// [`DriverConfig::max_concurrency`]). No registration transactions are
    /// submitted here; the [`Self::run`] loop spawns a dedicated task per
    /// registerable signer instead, so long Boundless proofs do not block the
    /// next discovery cycle.
    ///
    /// **Why no outer cancel-select.** `resolve_instance` performs several
    /// side effects before deciding whether an instance is registerable. The
    /// buffered stream is therefore drained to natural completion; each
    /// `resolve_instance` short-circuits on `self.config.cancel` between
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
    async fn discover_and_resolve(self: &Arc<Self>) -> Result<DiscoveryResolution> {
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

        let concurrency = self.config.max_concurrency.max(1);
        let mut futs = futures::stream::iter(instances.into_iter().map(|instance| {
            let driver = Arc::clone(self);
            let span = info_span!(
                "resolve_instance",
                instance_id = %instance.instance_id,
                endpoint = %instance.endpoint,
                health = ?instance.health_status,
            );
            async move {
                let result = driver.resolve_instance(&instance).await;
                (instance, result)
            }
            .instrument(span)
        }))
        .buffer_unordered(concurrency);

        // No cancel-select around `futs.next()`: each future checks
        // `self.config.cancel` cooperatively between awaits, so new work is
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
                    // Record signer -> instance attribution before the
                    // `instance` is moved into `RegisterableSigner` below.
                    // Consumed by `submit_deregistration` to annotate the
                    // dereg log with the last-known instance.
                    {
                        let mut history =
                            self.signer_history.lock().unwrap_or_else(|e| e.into_inner());
                        for addr in &outcome.addresses {
                            history.insert(*addr, instance.instance_id.clone());
                        }
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

    /// Drives the orphan-deregistration pass.
    ///
    /// Loads onchain signers, computes the orphan set (`registered \ active`),
    /// and deregisters each in sequence with a ghost-entry guard. The
    /// [`Self::run`] pipeline invokes this independently of the concurrent
    /// registration path.
    ///
    /// Both error paths (registry load and per-orphan deregistration)
    /// propagate uniformly so the caller can log + increment
    /// `processing_errors_total` once at a single site.
    async fn run_orphan_dereg(&self, protected_signers: &HashSet<Address>) -> Result<()> {
        // Cancel-aware: `get_registered_signers` is a side-effect-free
        // read, so dropping it on cancel is safe. Without this select,
        // a shutdown during the registry RPC would extend drain latency
        // by an entire round-trip before `deregister_orphans` is even
        // reached.
        let registered_signers = tokio::select! {
            biased;
            () = self.config.cancel.cancelled() => {
                debug!("cancelled before loading registered signers for orphan dereg");
                return Ok(());
            }
            res = self.registry.get_registered_signers() => res?,
        };
        self.deregister_orphans(protected_signers, &registered_signers).await
    }

    /// Builds the protected-signer set for the orphan-dereg pass: the
    /// union of `resolution.active_signers` and the keys of `pending`.
    ///
    /// Including `pending.keys()` closes the TOCTOU window described on
    /// [`Self::run`]: when an instance fails [`Self::resolve_instance`]
    /// transiently this cycle, its signer is absent from
    /// `active_signers`, but [`Self::reconcile_proof_tasks`] preserves
    /// the in-flight proof task (its instance id is in
    /// [`DiscoveryResolution::unresolved_instance_ids`]). If that task
    /// successfully registers the signer just as the orphan pass runs,
    /// the union ensures the freshly registered signer is treated as
    /// protected rather than as an orphan to be deregistered.
    fn protected_signers(
        resolution: &DiscoveryResolution,
        pending: &HashMap<Address, PendingRegistration>,
    ) -> HashSet<Address> {
        let mut protected = resolution.active_signers.clone();
        protected.extend(pending.keys().copied());
        protected
    }

    /// Reconciles the in-flight `pending` set against this cycle's
    /// `resolution`.
    ///
    /// Two passes:
    ///
    /// 1. **Cancel pass** — any task whose `signer` is no longer in the
    ///    current registerable set is cooperatively cancelled, **except**
    ///    when the task's `instance_id` is in
    ///    [`DiscoveryResolution::unresolved_instance_ids`]. That
    ///    inconclusive-snapshot guard prevents a single transient
    ///    `resolve_instance` failure (e.g. signer-service RPC blip, CRL
    ///    endpoint hiccup) from abandoning an in-flight ~70 min Boundless
    ///    proof: the signer is missing from `registerable` only because
    ///    we couldn't tell this cycle, not because we proved it's gone.
    ///    The `PendingRegistration::cancel` token fires; the task itself observes
    ///    it at its next checkpoint (proof generation, retry sleep,
    ///    pre-send) and exits with `Ok(())`. The entry stays in `pending`
    ///    until the join arrives (handled by [`Self::reap_finished_tasks`]).
    /// 2. **Spawn pass** — any registerable `(instance, signer)` not
    ///    currently in-flight (excluding already-cancelled tasks awaiting
    ///    reap) is spawned into the `JoinSet`. Each spawn creates a fresh
    ///    child token from [`DriverConfig::cancel`] so the parent
    ///    shutdown still propagates.
    ///
    /// Treating cancelled-but-not-reaped tasks as "not in-flight" enables
    /// single-cycle convergence for the vanish-then-reappear case (e.g.
    /// rolling deployments) where a signer drops out of `registerable`
    /// one cycle and returns the next: without this filter the fresh
    /// task would be deferred for an extra cycle until reap clears the
    /// stale entry. Safety relies on
    /// [`Self::in_flight_registrations`] (the registration-manager-layer
    /// process-wide `Mutex<HashSet<Address>>` dedupe) catching any
    /// brief overlap between the old task winding down and the new task
    /// entering registration. The second arrival short-circuits with a
    /// debug log and exits `Ok(())`.
    ///
    /// Transient task failures (non-cancel `Err`) are not re-spawned this
    /// cycle: the entry remains until reaped, after which the next cycle
    /// observes the empty in-flight set and respawns naturally if the
    /// signer is still registerable.
    fn reconcile_proof_tasks(
        self: &Arc<Self>,
        resolution: &DiscoveryResolution,
        tasks: &mut JoinSet<Result<Address>>,
        pending: &mut HashMap<Address, PendingRegistration>,
    ) {
        let wanted: HashSet<Address> = resolution.registerable.iter().map(|e| e.signer).collect();

        // Cancel-pass: any in-flight task whose signer is no longer
        // wanted AND whose source instance produced a conclusive
        // verdict this cycle (i.e. NOT in `unresolved_instance_ids`).
        // Tasks tied to instances that failed to resolve transiently
        // are preserved — the absence from `wanted` is then a lack of
        // evidence, not evidence of absence.
        //
        // `cancelled_by_reconcile = true` is set alongside the cancel
        // intent so [`Self::drain_proof_tasks`] can tell shutdown-driven
        // cancels (which it must count) apart from reconcile-driven
        // cancels (already counted here).
        for (signer, task) in pending.iter_mut() {
            if !wanted.contains(signer)
                && !task.cancel.is_cancelled()
                && !resolution.unresolved_instance_ids.contains(&task.instance_id)
            {
                info!(
                    signer = %signer,
                    instance = %task.instance_id,
                    "cancelling proof task: signer no longer registerable"
                );
                task.cancel.cancel();
                task.cancelled_by_reconcile = true;
                RegistrarMetrics::proof_tasks_cancelled().increment(1);
            } else if !wanted.contains(signer)
                && !task.cancel.is_cancelled()
                && resolution.unresolved_instance_ids.contains(&task.instance_id)
            {
                debug!(
                    signer = %signer,
                    instance = %task.instance_id,
                    "preserving proof task: source instance failed to resolve this cycle (inconclusive)"
                );
            }
        }

        // Build `in_flight` from only the still-live entries so a signer
        // that was cancelled in a previous cycle and has now reappeared
        // in `registerable` can spawn a fresh task immediately rather
        // than waiting two cycles (one to reap, one to respawn).
        // The registration-manager in-flight mutex catches any brief
        // overlap between the winding-down old task and the new task.
        // Updated as we spawn so a signer that appears in two registerable
        // entries within the same cycle (misconfig / discovery glitch —
        // two instances briefly backing the same enclave key) cannot
        // spawn duplicate proof tasks.
        //
        // A fresh spawn for a signer whose stale entry is still in
        // `pending` (cancelled, not yet reaped) overwrites the stale
        // entry below. The stale task continues running in the JoinSet
        // and `apply_join_outcome`'s task_id-match guard prevents it
        // from later evicting the fresh entry.
        let mut in_flight: HashSet<Address> = pending
            .iter()
            .filter(|(_, t)| !t.cancel.is_cancelled())
            .map(|(addr, _)| *addr)
            .collect();

        // Spawn-pass: any wanted signer not currently in-flight.
        for entry in &resolution.registerable {
            if !in_flight.insert(entry.signer) {
                continue;
            }
            let signer_cancel = self.config.cancel.child_token();
            let driver = Arc::clone(self);
            let instance_owned = entry.instance.clone();
            // Clone `instance_id` from `instance_owned` (rather than
            // re-reaching into `entry.instance`) to make the origin
            // explicit — the string is allocated twice either way
            // because `instance_owned` is moved into the spawned future
            // while `PendingRegistration` outlives the move.
            let instance_id = instance_owned.instance_id.clone();
            let attestation = entry.attestation.clone();
            let task_cancel = signer_cancel.clone();
            let signer = entry.signer;
            let enclave_index = entry.enclave_index;

            let handle = tasks.spawn(async move {
                driver
                    .run_proof_task(instance_owned, signer, enclave_index, attestation, task_cancel)
                    .await
            });
            pending.insert(
                signer,
                PendingRegistration {
                    instance_id,
                    task_id: handle.id(),
                    cancel: signer_cancel,
                    cancelled_by_reconcile: false,
                },
            );
            RegistrarMetrics::proof_tasks_spawned().increment(1);
        }
    }

    /// Spawned-task body: runs signer registration with task-scoped
    /// cancellation. Always returns `Ok(signer)` on cooperative cancel
    /// and on registration success; only genuine failures propagate as
    /// `Err`. The success arm carries the signer address so
    /// [`Self::apply_join_outcome`] can clean `pending` in O(1) without
    /// a reverse `task::Id → Address` lookup.
    async fn run_proof_task(
        self: Arc<Self>,
        instance: ProverInstance,
        signer: Address,
        enclave_index: usize,
        attestation_bytes: Vec<u8>,
        signer_cancel: CancellationToken,
    ) -> Result<Address> {
        let registration_manager = RegistrationManager::new(
            &self.proof_provider,
            &self.registry,
            &self.tx_manager,
            self.proof_semaphore.as_ref(),
            &self.in_flight_registrations,
            ProofHandlerConfig {
                registry_address: self.config.registry_address,
                max_tx_retries: self.config.max_tx_retries,
                tx_retry_delay: self.config.tx_retry_delay,
            },
        );
        registration_manager
            .register_signer(&instance, signer, enclave_index, &attestation_bytes, &signer_cancel)
            .await?;
        Ok(signer)
    }

    /// Drains every task that has already finished from `tasks`,
    /// removing the matching entry from `pending` and updating metrics
    /// via [`Self::apply_join_outcome`].
    ///
    /// Non-blocking: returns once `try_join_next_with_id` yields
    /// `None`. Called at the top of each [`Self::run`] cycle so the
    /// in-flight gauge tracks reality before the next reconcile.
    fn reap_finished_tasks(
        tasks: &mut JoinSet<Result<Address>>,
        pending: &mut HashMap<Address, PendingRegistration>,
    ) {
        while let Some(joined) = tasks.try_join_next_with_id() {
            Self::apply_join_outcome(Some(joined), pending);
        }
    }

    /// O(n) scan to find the signer address whose pending entry was
    /// spawned with `task_id`. `pending` is bounded by
    /// [`DriverConfig::max_concurrency`] (typically <20), so this
    /// rare-path scan (only hit on `Err` or panic, never the happy
    /// path) is cheaper than maintaining a second reverse `task::Id →
    /// Address` index that would have to stay consistent with every
    /// spawn/reap.
    fn find_signer_by_task_id(
        pending: &HashMap<Address, PendingRegistration>,
        task_id: task::Id,
    ) -> Option<Address> {
        pending.iter().find_map(|(addr, p)| (p.task_id == task_id).then_some(*addr))
    }

    /// Removes the `pending` entry for `signer` only when its
    /// `task_id` matches `id`. Returns the removed entry or `None` if
    /// the slot was already overwritten by a same-signer respawn.
    ///
    /// Every [`Self::apply_join_outcome`] arm funnels through this
    /// helper so the same stale-task / fresh-respawn invariant applies
    /// uniformly: a terminal outcome from a stale task must never
    /// evict the fresh entry reconcile dropped into the slot
    /// mid-cycle. The check is technically redundant on the
    /// [`Self::find_signer_by_task_id`] paths (the scan already filters
    /// by `task_id`), but making the guard local rather than implicit
    /// in another helper hardens the invariant against future
    /// refactors of the recovery routine.
    fn remove_if_task_matches(
        pending: &mut HashMap<Address, PendingRegistration>,
        signer: Address,
        id: task::Id,
    ) -> Option<PendingRegistration> {
        match pending.get(&signer) {
            Some(entry) if entry.task_id == id => pending.remove(&signer),
            _ => None,
        }
    }

    /// Consumes one `JoinSet` outcome and updates `pending` + metrics.
    ///
    /// Handles all three termination paths:
    /// - successful completion (`Ok((id, Ok(signer)))`) — the task
    ///   reported its signer directly, so cleanup is an O(1) lookup
    ///   guarded by a `task_id` match (a stale-but-still-running task
    ///   whose entry was already overwritten by a same-cycle respawn
    ///   must NOT evict the fresh entry).
    /// - inner error (`Ok((id, Err(_)))`) — no signer in hand, so the
    ///   address is recovered via [`Self::find_signer_by_task_id`].
    /// - join error (panic or external abort) — same recovery path,
    ///   keyed off [`tokio::task::JoinError::id`].
    ///
    /// Returns silently when `joined` is `None` so the caller's
    /// `try_join_next_with_id` loop can use it unconditionally.
    fn apply_join_outcome(
        joined: Option<std::result::Result<(task::Id, Result<Address>), tokio::task::JoinError>>,
        pending: &mut HashMap<Address, PendingRegistration>,
    ) {
        let Some(result) = joined else { return };
        RegistrarMetrics::proof_tasks_completed().increment(1);
        match result {
            Ok((id, Ok(signer))) => {
                let removed = Self::remove_if_task_matches(pending, signer, id);
                debug!(
                    task_id = ?id,
                    signer = %signer,
                    instance = ?removed.as_ref().map(|t| t.instance_id.as_str()),
                    superseded = removed.is_none(),
                    "proof task completed",
                );
            }
            Ok((id, Err(e))) => {
                let signer = Self::find_signer_by_task_id(pending, id);
                let removed = signer.and_then(|s| Self::remove_if_task_matches(pending, s, id));
                warn!(
                    task_id = ?id,
                    error = %e,
                    signer = ?signer,
                    instance = ?removed.as_ref().map(|t| t.instance_id.as_str()),
                    superseded = signer.is_some() && removed.is_none(),
                    "proof task failed"
                );
                RegistrarMetrics::processing_errors_total().increment(1);
            }
            Err(join_err) => {
                let id = join_err.id();
                let signer = Self::find_signer_by_task_id(pending, id);
                let removed = signer.and_then(|s| Self::remove_if_task_matches(pending, s, id));
                warn!(
                    task_id = ?id,
                    error = %join_err,
                    signer = ?signer,
                    instance = ?removed.as_ref().map(|t| t.instance_id.as_str()),
                    superseded = signer.is_some() && removed.is_none(),
                    "proof task join error (panic or abort)"
                );
                RegistrarMetrics::processing_errors_total().increment(1);
            }
        }
    }

    /// Cancels every pending task cooperatively, awaits them to natural
    /// completion, and updates `pending` via [`Self::apply_join_outcome`].
    /// Used only at shutdown — see [`Self::run`].
    ///
    /// **No `JoinSet::abort_all`.** Aborting would drop futures at arbitrary
    /// await points, including inside [`base_tx_manager::TxManager::send`]
    /// after a `NonceGuard` has been acquired but before the transaction is
    /// broadcast — leaving a permanent nonce gap (`NonceGuard::Drop` does not
    /// roll back). Cooperative cancellation is the only safe option: each
    /// task observes its `signer_cancel` token at its own checkpoints and
    /// exits with `Ok(())`. All registry RPCs in the spawned-task path
    /// (`is_registered`, `get_registered_signers`) are wrapped in
    /// `select!` against `signer_cancel` (or `DriverConfig::cancel` for
    /// non-spawned paths) so they drop immediately on cancel — the only
    /// remaining non-cancel-aware operation is
    /// [`base_tx_manager::TxManager::send`], which is intentionally
    /// kept that way to prevent the nonce-gap class of bugs. Shutdown
    /// latency is therefore bounded by a single in-flight `send()` per
    /// task, not by additional registry round-trips.
    async fn drain_proof_tasks(
        tasks: &mut JoinSet<Result<Address>>,
        pending: &mut HashMap<Address, PendingRegistration>,
    ) {
        for task in pending.values() {
            // Gate on `cancelled_by_reconcile`, NOT on
            // `task.cancel.is_cancelled()`. Each `signer_cancel` is a
            // child of `DriverConfig::cancel`, so by the time drain
            // runs the parent has already auto-cancelled every child —
            // `is_cancelled()` is `true` for all tasks regardless of
            // who triggered the cancel. Using the reconcile flag lets
            // us count every shutdown-driven cancellation exactly once
            // while preserving the "reconcile counted at intent time"
            // contract. `task.cancel.cancel()` is still issued for
            // belt-and-braces (it's a no-op when the parent fired) so
            // the bookkeeping stays correct if anyone ever decouples
            // `signer_cancel` from the parent in the future.
            if !task.cancelled_by_reconcile {
                task.cancel.cancel();
                RegistrarMetrics::proof_tasks_cancelled().increment(1);
            }
        }
        // NOTE: we drain through `join_next_with_id` (not
        // `JoinSet::shutdown`) so each terminal outcome flows through
        // `apply_join_outcome` — keeping the `pending` map and the
        // proof-task metrics consistent at shutdown.
        while let Some(joined) = tasks.join_next_with_id().await {
            Self::apply_join_outcome(Some(joined), pending);
        }
        RegistrarMetrics::proof_tasks_pending().set(0.0);
    }

    /// Submits a `deregisterSigner` transaction and returns whether it succeeded.
    async fn submit_deregistration(&self, signer: Address) -> bool {
        let calldata =
            Bytes::from(ITEEProverRegistry::deregisterSignerCall { signer }.abi_encode());

        // `last_known_instance = None` is the strongest single diagnostic
        // for phantom rotations: a signer we never observed in any
        // discovery cycle implies another registrar (or a prior
        // deployment) wrote it.
        let last_known_instance = {
            let history = self.signer_history.lock().unwrap_or_else(|e| e.into_inner());
            history.get(&signer).cloned()
        };
        info!(
            signer = %signer,
            last_known_instance = ?last_known_instance,
            registry = %self.config.registry_address,
            calldata_len = calldata.len(),
            "Deregistering signer"
        );

        let candidate = TxCandidate {
            tx_data: calldata,
            to: Some(self.config.registry_address),
            ..Default::default()
        };

        info!(
            tx = ?candidate,
            "Sending tx candidate",
        );

        match self.tx_manager.send(candidate).await {
            Ok(receipt) => {
                if !receipt.inner.status() {
                    warn!(
                        signer = %signer,
                        tx_hash = %receipt.transaction_hash,
                        "deregistration transaction reverted onchain",
                    );
                    RegistrarMetrics::processing_errors_total().increment(1);
                    return false;
                }
                info!(
                    signer = %signer,
                    tx_hash = %receipt.transaction_hash,
                    "signer deregistered"
                );
                true
            }
            Err(e) => {
                warn!(error = %e, signer = %signer, "failed to deregister signer");
                RegistrarMetrics::processing_errors_total().increment(1);
                false
            }
        }
    }

    /// Deregisters any onchain signer that is not in the `protected_signers` set.
    ///
    /// These orphans arise when a prover instance is terminated (e.g. ASG
    /// scale-down) without first deregistering its signer onchain. The
    /// `protected_signers` set is built by [`Self::protected_signers`] as
    /// the union of resolved-this-cycle signers and signers with an
    /// in-flight proof task, so transiently-unresolved instances and
    /// mid-flight registrations are both shielded from the sweep.
    ///
    /// # Defense in depth
    ///
    /// Before submitting a deregistration transaction, each orphan candidate is
    /// verified via [`RegistryClient::is_registered`] (backed by the
    /// `isRegisteredSigner` mapping). This guards against ghost entries in the
    /// onchain `EnumerableSetLib.AddressSet` that can appear after certain
    /// add/remove sequences due to a bug in Solady v0.0.245. Without this
    /// check, ghost addresses would be deregistered every cycle in an infinite
    /// loop, burning gas without effect.
    ///
    /// # Assumptions
    ///
    /// - **Single registrar**: This method queries *all* onchain signers and
    ///   treats any signer not in `protected_signers` as an orphan. If multiple
    ///   registrar instances manage disjoint prover fleets, one registrar would
    ///   incorrectly deregister another's signers. The current deployment model
    ///   assumes a single registrar per registry contract.
    async fn deregister_orphans(
        &self,
        protected_signers: &HashSet<Address>,
        registered_signers: &[Address],
    ) -> Result<()> {
        let orphans: Vec<_> = registered_signers
            .iter()
            .copied()
            .filter(|addr| !protected_signers.contains(addr))
            .collect();

        if orphans.is_empty() {
            return Ok(());
        }

        info!(count = orphans.len(), "deregistering orphan signers");

        let mut deregistered = 0usize;
        for signer in orphans {
            if self.config.cancel.is_cancelled() {
                debug!("shutdown requested, stopping orphan deregistration");
                break;
            }

            // Verify the signer is truly registered onchain before spending
            // gas on a deregistration tx. The `getRegisteredSigners()` view
            // reads from an `EnumerableSetLib.AddressSet` which can contain
            // ghost entries (addresses that appear in `values()` but have
            // `isRegisteredSigner == false`) due to a storage corruption bug
            // in Solady v0.0.245. Skipping ghosts prevents an infinite
            // deregistration loop.
            match self.registry.is_registered(signer).await {
                Ok(false) => {
                    warn!(
                        signer = %signer,
                        "signer appears in getRegisteredSigners but isRegisteredSigner is false, \
                         skipping (possible EnumerableSet ghost entry)"
                    );
                    continue;
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        signer = %signer,
                        "failed to verify signer registration status, skipping deregistration"
                    );
                    continue;
                }
                Ok(true) => {}
            }

            if self.submit_deregistration(signer).await {
                RegistrarMetrics::deregistrations_total().increment(1);
                deregistered += 1;
            }
        }

        info!(count = deregistered, "orphan deregistration complete");
        Ok(())
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
    use alloy_primitives::{Address, B256, Bloom, Bytes, address};
    use alloy_rpc_types_eth::TransactionReceipt;
    use alloy_sol_types::SolCall;
    use async_trait::async_trait;
    use base_proof_tee_nitro_attestation_prover::AttestationProof;
    use base_tx_manager::{SendHandle, TxCandidate, TxManager};
    use hex_literal::hex;
    use k256::ecdsa::SigningKey;
    use rstest::rstest;
    use tokio_util::sync::CancellationToken;
    use url::Url;

    use super::*;
    use crate::{InstanceHealthStatus, RegistryClient, Result, SignerClient};

    // ── Shared constants ────────────────────────────────────────────────

    /// Expected byte length of ABI-encoded `deregisterSigner(address)` calldata:
    /// 4-byte selector + 32-byte left-padded address word.
    const DEREGISTER_CALLDATA_LEN: usize = 36;

    /// Number of zero-padding bytes before the 20-byte address in the ABI word.
    const ABI_ADDRESS_PAD: usize = 12;

    /// Byte offset where the raw 20-byte address starts in the encoded calldata
    /// (after the 4-byte selector and 12 bytes of zero-padding).
    const ABI_ADDRESS_OFFSET: usize = 4 + ABI_ADDRESS_PAD;

    /// Well-known Hardhat / Anvil account #0 address.
    const HARDHAT_ACCOUNT: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    /// Well-known Hardhat / Anvil account #0 private key.
    const HARDHAT_KEY_0: [u8; 32] =
        hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

    /// Hardhat / Anvil account #1 private key.
    const HARDHAT_KEY_1: [u8; 32] =
        hex!("59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d");

    /// Hardhat / Anvil account #2 private key.
    const HARDHAT_KEY_2: [u8; 32] =
        hex!("5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a");

    /// Hardhat / Anvil account #3 private key.
    const HARDHAT_KEY_3: [u8; 32] =
        hex!("7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6");

    /// Prover instance endpoints for tests. Each simulates a distinct
    /// EC2 instance at a private IP.
    const EP1: &str = "10.0.0.1:8000";
    const EP2: &str = "10.0.0.2:8000";
    const EP3: &str = "10.0.0.3:8000";
    const EP4: &str = "10.0.0.4:8000";

    /// Synthetic orphan addresses for deregistration tests.
    /// Each uses `Address::repeat_byte` for deterministic, readable values.
    const ORPHAN_A: Address = Address::repeat_byte(0xAA);
    const ORPHAN_B: Address = Address::repeat_byte(0xBB);
    const ORPHAN_C: Address = Address::repeat_byte(0xCC);
    const ORPHAN_D: Address = Address::repeat_byte(0xDD);
    const ORPHAN_E: Address = Address::repeat_byte(0xEE);

    /// Placeholder registry contract address used in `DriverConfig`.
    const TEST_REGISTRY_ADDRESS: Address = Address::repeat_byte(0x01);

    // ── Test helpers ─────────────────────────────────────────────────────

    /// Derives the uncompressed 65-byte public key from a private key.
    fn public_key_from_private(private_key: &[u8; 32]) -> Vec<u8> {
        let signing_key = SigningKey::from_slice(private_key).unwrap();
        signing_key.verifying_key().to_encoded_point(false).as_bytes().to_vec()
    }

    /// Builds a minimal `TransactionReceipt` for mock tx managers.
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

    /// Builds a [`ProverInstance`] with the given host:port and health status.
    ///
    /// Prepends `http://` to form a valid URL automatically. The `launch_time`
    /// defaults to `None` — use [`instance_with_launch_time`] for tests that
    /// need a specific launch time.
    fn instance(host_port: &str, status: InstanceHealthStatus) -> ProverInstance {
        let endpoint = Url::parse(&format!("http://{host_port}")).unwrap();
        ProverInstance {
            instance_id: format!("i-{host_port}"),
            endpoint,
            health_status: status,
            launch_time: None,
        }
    }

    /// Builds a [`ProverInstance`] with an explicit `launch_time`.
    fn instance_with_launch_time(
        host_port: &str,
        status: InstanceHealthStatus,
        launch_time: Option<SystemTime>,
    ) -> ProverInstance {
        let endpoint = Url::parse(&format!("http://{host_port}")).unwrap();
        ProverInstance {
            instance_id: format!("i-{host_port}"),
            endpoint,
            health_status: status,
            launch_time,
        }
    }

    // ── Mock implementations ────────────────────────────────────────────

    /// Configurable mock discovery that returns a pre-set list of instances.
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

    /// Mock proof provider that returns a dummy proof.
    #[derive(Debug)]
    struct StubProofProvider;

    #[async_trait]
    impl AttestationProofProvider for StubProofProvider {
        async fn generate_proof(
            &self,
            _attestation_bytes: &[u8],
            _cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            Ok(AttestationProof {
                output: Bytes::from_static(b"stub-output"),
                proof_bytes: Bytes::from_static(b"stub-proof"),
            })
        }
    }

    /// Mock proof provider that always fails, simulating Boundless errors.
    #[derive(Debug)]
    struct FailingProofProvider;

    #[async_trait]
    impl AttestationProofProvider for FailingProofProvider {
        async fn generate_proof(
            &self,
            _attestation_bytes: &[u8],
            _cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            Err(base_proof_tee_nitro_attestation_prover::ProverError::Boundless(
                "simulated proof failure".into(),
            ))
        }
    }

    /// Mock signer client that returns pre-configured public keys and attestations
    /// per endpoint.
    ///
    /// If an endpoint is not in the `keys` map, the call returns an error
    /// (simulating an unreachable instance).
    #[derive(Debug)]
    struct MockSignerClient {
        /// Maps endpoint URL → list of uncompressed public key bytes (one per enclave).
        keys: HashMap<Url, Vec<Vec<u8>>>,
        /// Maps endpoint URL → list of attestation blobs (one per enclave).
        /// Falls back to `b"mock-attestation"` if not configured.
        attestations: HashMap<Url, Vec<Vec<u8>>>,
        /// Endpoints whose attestation RPC should fail.
        fail_attestation: HashSet<Url>,
    }

    impl MockSignerClient {
        /// Creates a mock with the given host:port-to-private-key mappings.
        /// Each endpoint gets a single enclave key wrapped in a Vec.
        /// The public key is derived automatically from each private key.
        /// An `http://` scheme is prepended to each host:port string.
        fn from_keys(entries: &[(&str, &[u8; 32])]) -> Self {
            let keys = entries
                .iter()
                .map(|(ep, pk)| {
                    let url = Url::parse(&format!("http://{ep}")).unwrap();
                    (url, vec![public_key_from_private(pk)])
                })
                .collect();
            Self { keys, attestations: HashMap::new(), fail_attestation: HashSet::new() }
        }

        /// Creates a mock that returns multiple public keys for a single endpoint,
        /// simulating a multi-enclave instance.
        fn multi_enclave(host_port: &str, private_keys: &[&[u8; 32]]) -> Self {
            let url = Url::parse(&format!("http://{host_port}")).unwrap();
            let pubs = private_keys.iter().map(|pk| public_key_from_private(pk)).collect();
            Self {
                keys: HashMap::from([(url, pubs)]),
                attestations: HashMap::new(),
                fail_attestation: HashSet::new(),
            }
        }

        /// Configures attestation blobs for a given endpoint.
        fn with_attestations(mut self, host_port: &str, attestations: Vec<Vec<u8>>) -> Self {
            let url = Url::parse(&format!("http://{host_port}")).unwrap();
            self.attestations.insert(url, attestations);
            self
        }

        /// Configures the attestation RPC for a given endpoint to fail.
        fn with_attestation_failure(mut self, host_port: &str) -> Self {
            let url = Url::parse(&format!("http://{host_port}")).unwrap();
            self.fail_attestation.insert(url);
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
            // Default: one dummy attestation per key at this endpoint.
            let count = self.keys.get(endpoint).map_or(1, |k| k.len());
            Ok(vec![b"mock-attestation".to_vec(); count])
        }
    }

    /// Mock registry that returns a configured set of registered signers.
    ///
    /// By default, `is_registered` checks membership in the `signers` list
    /// (matching real contract behavior). When `all_registered` is set, it
    /// returns `true` unconditionally, which short-circuits the registration
    /// path after the registry precheck.
    #[derive(Debug)]
    struct MockRegistry {
        signers: Vec<Address>,
        /// When `true`, `is_registered` returns `true` for all queries,
        /// regardless of `signers` membership.
        all_registered: bool,
    }

    impl MockRegistry {
        fn with_signers(signers: Vec<Address>) -> Self {
            Self { signers, all_registered: false }
        }

        fn all_registered(signers: Vec<Address>) -> Self {
            Self { signers, all_registered: true }
        }
    }

    #[async_trait]
    impl RegistryClient for MockRegistry {
        async fn is_registered(&self, signer: Address) -> Result<bool> {
            if self.all_registered {
                return Ok(true);
            }
            Ok(self.signers.contains(&signer))
        }

        async fn get_registered_signers(&self) -> Result<Vec<Address>> {
            Ok(self.signers.clone())
        }
    }

    /// Mock tx manager that records submitted calldata for assertion.
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
            unimplemented!("not used in tests")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    /// Stub signer client that is unused by `deregister_orphans` tests.
    #[derive(Debug)]
    struct StubSignerClient;

    #[async_trait]
    impl SignerClient for StubSignerClient {
        async fn signer_public_key(&self, _endpoint: &Url) -> Result<Vec<Vec<u8>>> {
            unimplemented!("not used in deregister_orphans tests")
        }

        async fn signer_attestation(
            &self,
            _endpoint: &Url,
            _user_data: Option<Vec<u8>>,
            _nonce: Option<Vec<u8>>,
        ) -> Result<Vec<Vec<u8>>> {
            unimplemented!("not used in deregister_orphans tests")
        }
    }

    // ── Driver constructors ─────────────────────────────────────────────

    fn default_config(cancel: CancellationToken) -> DriverConfig {
        DriverConfig {
            registry_address: TEST_REGISTRY_ADDRESS,
            poll_interval: Duration::from_secs(1),
            cancel,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            max_tx_retries: DEFAULT_MAX_TX_RETRIES,
            tx_retry_delay: Duration::from_secs(DEFAULT_TX_RETRY_DELAY_SECS),
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

    /// Builds a driver for `deregister_orphans` tests (no signer client needed).
    fn driver_with_shared_tx(
        registered_signers: Vec<Address>,
        tx: SharedTxManager,
    ) -> RegistrationDriver<
        MockDiscovery,
        StubProofProvider,
        MockRegistry,
        SharedTxManager,
        StubSignerClient,
    > {
        let registry = MockRegistry::with_signers(registered_signers);
        RegistrationDriver::new(
            MockDiscovery { instances: vec![] },
            StubProofProvider,
            registry,
            tx,
            StubSignerClient,
            default_config(CancellationToken::new()),
            None,
        )
        .expect("test driver construction succeeds")
    }

    /// Builds a fully-configured driver for primitive-level tests that
    /// invoke `discover_and_resolve` and `run_orphan_dereg` directly
    /// (rather than the spawn pipeline in `run`). Returns an `Arc` so callers can invoke
    /// `discover_and_resolve` (which takes `&Arc<Self>`) without
    /// re-wrapping at every call site.
    fn cycle_driver(
        instances: Vec<ProverInstance>,
        signer_client: MockSignerClient,
        registry: MockRegistry,
        tx: SharedTxManager,
        cancel: CancellationToken,
    ) -> Arc<
        RegistrationDriver<
            MockDiscovery,
            StubProofProvider,
            MockRegistry,
            SharedTxManager,
            MockSignerClient,
        >,
    > {
        Arc::new(
            RegistrationDriver::new(
                MockDiscovery { instances },
                StubProofProvider,
                registry,
                tx,
                signer_client,
                default_config(cancel),
                None,
            )
            .expect("test driver construction succeeds"),
        )
    }

    // ── Proof-task mock types ─────────────────────────────────────────

    /// Proof provider that records the `(signer, attestation_bytes)` pair
    /// passed to every `generate_proof_for_signer` invocation, then
    /// returns `Err` so the spawned registration task exits without
    /// reaching the tx-manager send path.
    ///
    /// Used by the spawn-pass indexing tests to assert that
    /// [`RegistrationDriver::reconcile_proof_tasks`] pairs each signer
    /// with `attestations[idx]` and never with a sibling's blob.
    #[derive(Debug, Clone, Default)]
    struct RecordingProofProvider {
        recorded: Arc<Mutex<HashMap<Address, Vec<u8>>>>,
    }

    impl RecordingProofProvider {
        fn snapshot(&self) -> HashMap<Address, Vec<u8>> {
            self.recorded.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AttestationProofProvider for RecordingProofProvider {
        async fn generate_proof(
            &self,
            _attestation_bytes: &[u8],
            _cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            unreachable!(
                "RecordingProofProvider is only invoked via generate_proof_for_signer; \
                 reaching generate_proof would mean the driver bypassed signer routing"
            )
        }

        async fn generate_proof_for_signer(
            &self,
            attestation_bytes: &[u8],
            signer_address: Address,
            _cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            self.recorded.lock().unwrap().insert(signer_address, attestation_bytes.to_vec());
            // Returning `Err` short-circuits the spawned task before it reaches
            // `tx_manager.send()`, which we do not wire for the indexing tests.
            Err(base_proof_tee_nitro_attestation_prover::ProverError::Boundless(
                "RecordingProofProvider exits after capturing attestation".into(),
            ))
        }
    }

    // ── Pipeline test infrastructure ────────────────────────────────────
    //
    // Used by the `reconcile_proof_tasks`, `reap_finished_tasks`, and
    // top-level `run` test suites added in this commit. Designed so the
    // entire run loop can be driven from a `tokio::test` without real
    // sleeps: the gated proof provider parks every spawned proof on a
    // `CancellationToken` that the test releases when it has observed
    // the behaviour it cares about.

    /// Tightened poll interval for spawn-pipeline tests so we observe
    /// multiple cycles without burning real wall-time.
    const GATED_POLL_INTERVAL: Duration = Duration::from_millis(25);

    /// Soft timeout for `wait_for` polling assertions. Generous enough
    /// to absorb CI jitter while still failing fast on a stuck pipeline.
    const GATED_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

    /// Minimum number of run cycles a pipeline test must let elapse
    /// before asserting that discovery is unblocked. Two cycles proves
    /// the loop is not blocked behind a long proof (one would also pass
    /// for a synchronous loop on its first iteration).
    const MIN_CYCLES_IN_OBSERVATION_WINDOW: usize = 2;

    /// Shared mutable state for [`GatedProofProvider`].
    ///
    /// `release` is a [`CancellationToken`] — not a [`tokio::sync::Notify`]
    /// — so it is *latched*. A late-arriving proof task that calls
    /// `generate_proof_for_signer` after `release_all()` returns
    /// immediately, instead of missing a one-shot `notify_waiters` wakeup
    /// and hanging forever. This eliminates a registration race the
    /// earlier design suffered from.
    #[derive(Debug, Default)]
    struct GatedProofState {
        release: CancellationToken,
        call_count: AtomicUsize,
        in_flight: AtomicUsize,
        /// Optional per-signer failure routing. If a signer address is
        /// present in this set, [`GatedProofProvider::generate_proof_for_signer`]
        /// returns a synthetic [`ProverError::Boundless`] immediately
        /// (skipping the release gate) so tests can observe the registration
        /// failure path without having to stand up a second proof provider type. The check happens
        /// before [`GatedProofState::call_count`] is incremented, so
        /// failed signers do not contribute to the in-flight count
        /// either.
        fail_for: Mutex<HashSet<Address>>,
    }

    /// RAII guard that bumps and decrements [`GatedProofState::in_flight`].
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

    /// Proof provider that parks every call on a shared cancel token
    /// until the test releases it, while tracking call and in-flight
    /// counts (see [`GatedProofHandles::call_count`] and
    /// [`GatedProofHandles::in_flight`]).
    ///
    /// Cancel-safe: the await on `release.cancelled()` is itself cancellable,
    /// so when the registration manager's biased `select!` drops this future
    /// on its own `signer_cancel`, no state is left hanging.
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
            _cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            self.state.call_count.fetch_add(1, Ordering::SeqCst);
            let _guard = InFlightGuard::new(Arc::clone(&self.state));
            self.state.release.cancelled().await;
            Ok(AttestationProof {
                output: Bytes::from_static(b"gated-output"),
                proof_bytes: Bytes::from_static(b"gated-proof"),
            })
        }

        async fn generate_proof_for_signer(
            &self,
            attestation_bytes: &[u8],
            signer_address: Address,
            cancel: &CancellationToken,
        ) -> base_proof_tee_nitro_attestation_prover::Result<AttestationProof> {
            if self.state.fail_for.lock().unwrap().contains(&signer_address) {
                return Err(base_proof_tee_nitro_attestation_prover::ProverError::Boundless(
                    "synthetic failure injected by GatedProofProvider".into(),
                ));
            }
            self.generate_proof(attestation_bytes, cancel).await
        }
    }

    /// Test-side handle for inspecting and releasing a [`GatedProofProvider`].
    #[derive(Debug, Clone)]
    struct GatedProofHandles {
        state: Arc<GatedProofState>,
    }

    impl GatedProofHandles {
        /// Releases every currently-blocked and every future proof call
        /// (the token is latched).
        fn release_all(&self) {
            self.state.release.cancel();
        }

        fn call_count(&self) -> usize {
            self.state.call_count.load(Ordering::SeqCst)
        }

        fn in_flight(&self) -> usize {
            self.state.in_flight.load(Ordering::SeqCst)
        }

        /// Configures [`GatedProofProvider::generate_proof_for_signer`]
        /// to return a synthetic [`ProverError::Boundless`] for the
        /// given signer addresses on every subsequent call. The check
        /// happens **before** the gate, so failing tasks do not affect
        /// [`Self::call_count`] or [`Self::in_flight`].
        fn fail_for_signers(&self, signers: impl IntoIterator<Item = Address>) {
            self.state.fail_for.lock().unwrap().extend(signers);
        }
    }

    /// Discovery whose returned instance list can be mutated mid-run.
    ///
    /// Lets a test simulate ASG scale-up/down between cycles without
    /// restarting the driver.
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

    /// Type alias for the [`RegistrationDriver`] specialisation used by
    /// the pipeline tests.
    type RunDriver = RegistrationDriver<
        MutableDiscovery,
        GatedProofProvider,
        MockRegistry,
        SharedTxManager,
        MockSignerClient,
    >;

    /// Bundles every handle a pipeline test needs to drive the loop.
    struct GatedRunHarness {
        driver: Arc<RunDriver>,
        cancel: CancellationToken,
        discovery: MutableDiscovery,
        proof: GatedProofHandles,
        tx: SharedTxManager,
    }

    impl GatedRunHarness {
        /// Builds a harness with the given initial instances + signer
        /// keys; both come from `endpoints_to_keys` so the wiring
        /// (instance ↔ key ↔ derived address) cannot drift.
        fn new(
            initial_instances: Vec<ProverInstance>,
            endpoints_to_keys: &[(&str, &[u8; 32])],
            registry: MockRegistry,
        ) -> Self {
            let discovery = MutableDiscovery::new(initial_instances);
            let signer_client = MockSignerClient::from_keys(endpoints_to_keys);
            let tx = SharedTxManager::new();
            let cancel = CancellationToken::new();
            let (proof_provider, proof_handles) = GatedProofProvider::new();

            let mut config = default_config(cancel.clone());
            config.poll_interval = GATED_POLL_INTERVAL;

            let driver = Arc::new(
                RegistrationDriver::new(
                    discovery.clone(),
                    proof_provider,
                    registry,
                    tx.clone(),
                    signer_client,
                    config,
                    None,
                )
                .expect("test driver construction succeeds"),
            );

            Self { driver, cancel, discovery, proof: proof_handles, tx }
        }

        /// Spawns the registration loop on the current runtime, returning
        /// the `JoinHandle` so the test can await shutdown. Uses
        /// [`RegistrationDriver::run_arc`] (rather than the value-API
        /// `run`) so the harness can keep its own `Arc<RegistrationDriver>`
        /// for state inspection.
        fn spawn_run(&self) -> tokio::task::JoinHandle<Result<()>> {
            let driver = Arc::clone(&self.driver);
            tokio::spawn(driver.run_arc())
        }

        /// Cancels the harness, awaits its run handle inside
        /// [`GATED_WAIT_TIMEOUT`], and asserts the loop exited cleanly.
        /// Every pipeline test must call this exactly once at the end
        /// to drain in-flight proof tasks and surface unexpected panics.
        async fn shutdown(&self, handle: tokio::task::JoinHandle<Result<()>>) {
            self.cancel.cancel();
            let outcome = tokio::time::timeout(GATED_WAIT_TIMEOUT, handle)
                .await
                .expect("run() should observe cancel and stop within timeout")
                .expect("run() task should not panic");
            outcome.expect("run() should return Ok on graceful shutdown");
        }
    }

    /// Polls `predicate` until it returns `true` or [`GATED_WAIT_TIMEOUT`]
    /// elapses. Panics with `label` on timeout so test failures point at
    /// the specific expectation that didn't fire.
    async fn wait_for(label: &str, predicate: impl Fn() -> bool) {
        let started = std::time::Instant::now();
        while !predicate() {
            if started.elapsed() > GATED_WAIT_TIMEOUT {
                panic!("timed out waiting for: {label}");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Counts the number of `registerSigner` calldata frames in a
    /// captured tx-manager log.
    fn count_register_calls(sent: &[Bytes]) -> usize {
        let sel = ITEEProverRegistry::registerSignerCall::SELECTOR;
        sent.iter().filter(|c| c.len() >= 4 && c[..4] == sel).count()
    }

    /// Counts the number of `deregisterSigner` calldata frames.
    fn count_deregister_calls(sent: &[Bytes]) -> usize {
        let sel = ITEEProverRegistry::deregisterSignerCall::SELECTOR;
        sent.iter().filter(|c| c.len() >= 4 && c[..4] == sel).count()
    }

    /// Instance ID used by every `PendingRegistration` constructed for
    /// the reconcile / reap unit tests. The string is opaque — only the
    /// `Address` keying matters for cancel/spawn logic — but pinning it
    /// to a single named const keeps test output readable and avoids
    /// per-test magic strings.
    const TEST_PENDING_INSTANCE_ID: &str = "i-pending-test";

    /// Cooperative shutdown for any [`JoinSet`] / `pending` pair built
    /// by a unit test (i.e. without spawning the full `run()` loop).
    ///
    /// Fires every per-task cancel token, then `abort_all`s as a
    /// backstop and drains the `JoinSet` so test teardown doesn't leak
    /// futures that are forever parked on their tokens. Mirrors the
    /// production shutdown sequence in [`RegistrationDriver::run`].
    async fn drain_test_tasks(
        tasks: &mut JoinSet<Result<Address>>,
        pending: &mut HashMap<Address, PendingRegistration>,
    ) {
        for task in pending.values() {
            task.cancel.cancel();
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        pending.clear();
    }

    /// Polling fixed point used by the reap-loop helper below — small
    /// enough that even on a loaded runner the test still terminates
    /// well inside [`GATED_WAIT_TIMEOUT`].
    const REAP_POLL_INTERVAL: Duration = Duration::from_millis(1);

    /// Repeatedly invokes [`RunDriver::reap_finished_tasks`] until
    /// `pending` is empty or [`GATED_WAIT_TIMEOUT`] elapses. The
    /// production loop calls `reap_finished_tasks` exactly once per
    /// cycle, so unit tests that drive it directly need to give the
    /// runtime time to schedule the spawned task that they are waiting
    /// to observe complete.
    async fn reap_until_pending_empty(
        tasks: &mut JoinSet<Result<Address>>,
        pending: &mut HashMap<Address, PendingRegistration>,
    ) {
        let started = std::time::Instant::now();
        while !pending.is_empty() {
            if started.elapsed() > GATED_WAIT_TIMEOUT {
                panic!("timed out reaping {} pending task(s)", pending.len());
            }
            RunDriver::reap_finished_tasks(tasks, pending);
            tokio::time::sleep(REAP_POLL_INTERVAL).await;
        }
    }

    /// Builds a [`MockRegistry`] that reports zero registered signers.
    /// Used by every pipeline test that wants the onchain state to
    /// start empty.
    fn empty_registry() -> MockRegistry {
        MockRegistry::with_signers(vec![])
    }

    /// Builds the single-instance / single-key `(EP1, HARDHAT_KEY_0)`
    /// harness used by most pipeline tests, with no signers registered
    /// onchain. Centralising this pair removes ~5 lines of boilerplate
    /// per test and makes it impossible for them to drift out of sync.
    fn single_healthy_harness() -> GatedRunHarness {
        GatedRunHarness::new(
            vec![instance(EP1, InstanceHealthStatus::Healthy)],
            &[(EP1, &HARDHAT_KEY_0)],
            empty_registry(),
        )
    }

    /// Builds an `n`-instance harness with all healthy instances and
    /// no signers registered onchain. The signer client is seeded with
    /// **every** endpoint in [`ALL_ENDPOINTS`] (not just the initial
    /// `n`) so scale-up tests that swap the discovery list mid-run can
    /// resolve public keys for instances that weren't part of the
    /// original snapshot.
    fn multi_healthy_harness(num_instances: usize) -> GatedRunHarness {
        assert!(
            num_instances <= ALL_ENDPOINTS.len(),
            "fixture has only {} endpoints; requested {num_instances}",
            ALL_ENDPOINTS.len()
        );
        let initial: Vec<_> = ALL_ENDPOINTS[..num_instances]
            .iter()
            .map(|ep| instance(ep, InstanceHealthStatus::Healthy))
            .collect();
        let all_keys: Vec<(&str, &[u8; 32])> =
            ALL_ENDPOINTS.iter().copied().zip(ALL_KEYS.iter().copied()).collect();
        GatedRunHarness::new(initial, &all_keys, empty_registry())
    }

    /// Builds a minimal [`PendingRegistration`] for unit-testing
    /// reap/apply-outcome flows without spawning a real future. The
    /// `task_id` is taken from the spawned placeholder's
    /// `JoinHandle::id()` so the failure-path O(n) scan in
    /// [`RegistrationDriver::apply_join_outcome`] can recover the
    /// signer just as it does in production.
    fn pending_registration_for_test(task_id: task::Id, instance_id: &str) -> PendingRegistration {
        PendingRegistration {
            instance_id: instance_id.to_string(),
            task_id,
            cancel: CancellationToken::new(),
            cancelled_by_reconcile: false,
        }
    }

    /// Builds a synthetic [`DiscoveryResolution`] from a list of
    /// `(endpoint, key)` pairs we want kept as registerable this
    /// cycle.
    fn dr_from_kept(kept: &[(&str, &[u8; 32])]) -> DiscoveryResolution {
        let mut registerable = Vec::new();
        let mut active_signers = HashSet::new();
        for (ep, key) in kept {
            let inst = instance(ep, InstanceHealthStatus::Healthy);
            let addr = ProverClient::derive_address(&public_key_from_private(key)).unwrap();
            active_signers.insert(addr);
            registerable.push(RegisterableSigner {
                instance: inst,
                signer: addr,
                attestation: b"gated-attestation".to_vec(),
                enclave_index: 0,
            });
        }
        let total = kept.len();
        DiscoveryResolution {
            registerable,
            active_signers,
            reachable_count: total,
            total_count: total,
            ok_to_dereg: true,
            unresolved_instance_ids: HashSet::new(),
        }
    }

    // ── Calldata encoding tests ─────────────────────────────────────────

    #[rstest]
    #[case::zero_address(Address::ZERO)]
    #[case::hardhat_account(HARDHAT_ACCOUNT)]
    #[case::all_ones(Address::repeat_byte(0xFF))]
    fn deregister_calldata_encodes_correctly(#[case] signer: Address) {
        let calldata = ITEEProverRegistry::deregisterSignerCall { signer }.abi_encode();

        assert_eq!(calldata.len(), DEREGISTER_CALLDATA_LEN);
        assert_eq!(&calldata[..4], &ITEEProverRegistry::deregisterSignerCall::SELECTOR);
        // The 12 bytes between the selector and the address must be zero-padding.
        assert_eq!(&calldata[4..ABI_ADDRESS_OFFSET], &[0u8; ABI_ADDRESS_PAD]);
        // The last 20 bytes must be the raw signer address.
        assert_eq!(&calldata[ABI_ADDRESS_OFFSET..], signer.as_slice());
    }

    // ── deregister_orphans tests ────────────────────────────────────────

    #[rstest]
    #[case::no_orphans(vec![ORPHAN_A, ORPHAN_B], vec![ORPHAN_A, ORPHAN_B], 0)]
    #[case::one_orphan(vec![ORPHAN_A, ORPHAN_B], vec![ORPHAN_A], 1)]
    #[case::all_orphans(vec![ORPHAN_A, ORPHAN_B], vec![], 2)]
    #[tokio::test]
    async fn deregister_orphans_tx_count(
        #[case] registered: Vec<Address>,
        #[case] active: Vec<Address>,
        #[case] expected_txs: usize,
    ) {
        let active: HashSet<Address> = active.into_iter().collect();

        let tx = SharedTxManager::new();
        let driver = driver_with_shared_tx(registered.clone(), tx.clone());

        driver.deregister_orphans(&active, &registered).await.unwrap();

        assert_eq!(tx.sent_calldata().len(), expected_txs);
    }

    #[tokio::test]
    async fn deregister_orphans_calldata_targets_orphan() {
        let registered = vec![ORPHAN_A, ORPHAN_B];
        let tx = SharedTxManager::new();
        let driver = driver_with_shared_tx(registered.clone(), tx.clone());

        driver.deregister_orphans(&HashSet::from([ORPHAN_A]), &registered).await.unwrap();

        let sent = tx.sent_calldata();
        let expected = ITEEProverRegistry::deregisterSignerCall { signer: ORPHAN_B }.abi_encode();
        assert_eq!(sent[0], Bytes::from(expected));
    }

    #[tokio::test]
    async fn deregister_orphans_respects_cancellation() {
        let tx = SharedTxManager::new();
        let cancel = CancellationToken::new();
        let registry = MockRegistry::with_signers(vec![ORPHAN_A]);
        let driver = RegistrationDriver::new(
            MockDiscovery { instances: vec![] },
            StubProofProvider,
            registry,
            tx.clone(),
            StubSignerClient,
            default_config(cancel.clone()),
            None,
        )
        .expect("test driver construction succeeds");

        let registered = vec![ORPHAN_A];
        cancel.cancel();
        driver.deregister_orphans(&HashSet::new(), &registered).await.unwrap();

        assert!(tx.sent_calldata().is_empty(), "no txs should be sent after cancellation");
    }

    /// Mock registry that simulates a corrupted `EnumerableSetLib.AddressSet`.
    ///
    /// `get_registered_signers()` returns `all_values` (including ghost entries),
    /// but `is_registered()` only returns `true` for addresses in
    /// `truly_registered`. This models the Solady v0.0.245 bug where
    /// `values()` contains stale addresses whose `isRegisteredSigner`
    /// mapping is `false`.
    #[derive(Debug)]
    struct GhostRegistry {
        /// Addresses returned by `getRegisteredSigners()` (includes ghosts).
        all_values: Vec<Address>,
        /// Addresses for which `isRegisteredSigner` is `true`.
        truly_registered: HashSet<Address>,
    }

    impl GhostRegistry {
        /// Creates a registry where `ghosts` appear in `values()` but have
        /// `isRegisteredSigner == false`, and `real` signers appear in both.
        fn new(real: Vec<Address>, ghosts: Vec<Address>) -> Self {
            let truly_registered: HashSet<Address> = real.iter().copied().collect();
            let mut all_values = real;
            all_values.extend(ghosts);
            Self { all_values, truly_registered }
        }
    }

    #[async_trait]
    impl RegistryClient for GhostRegistry {
        async fn is_registered(&self, signer: Address) -> Result<bool> {
            Ok(self.truly_registered.contains(&signer))
        }

        async fn get_registered_signers(&self) -> Result<Vec<Address>> {
            Ok(self.all_values.clone())
        }
    }

    #[tokio::test]
    async fn deregister_orphans_skips_ghost_entries() {
        // Simulates the Solady v0.0.245 EnumerableSetLib bug: ORPHAN_A is a
        // ghost entry that appears in getRegisteredSigners() but has
        // isRegisteredSigner == false. ORPHAN_B is a real orphan.
        let ghost_registry = GhostRegistry::new(vec![ORPHAN_B], vec![ORPHAN_A]);

        let tx = SharedTxManager::new();
        let driver = RegistrationDriver::new(
            MockDiscovery { instances: vec![] },
            StubProofProvider,
            ghost_registry,
            tx.clone(),
            StubSignerClient,
            default_config(CancellationToken::new()),
            None,
        )
        .expect("test driver construction succeeds");

        // Both ORPHAN_A and ORPHAN_B are "registered" (in values()),
        // neither is in active_signers.
        let registered = vec![ORPHAN_A, ORPHAN_B];
        driver.deregister_orphans(&HashSet::new(), &registered).await.unwrap();

        let sent = tx.sent_calldata();
        // Only ORPHAN_B should be deregistered; ORPHAN_A is a ghost.
        assert_eq!(sent.len(), 1, "ghost entry should be skipped");
        let expected = ITEEProverRegistry::deregisterSignerCall { signer: ORPHAN_B }.abi_encode();
        assert_eq!(sent[0], Bytes::from(expected));
    }

    #[tokio::test]
    async fn deregister_orphans_skips_all_ghosts_sends_nothing() {
        // All orphan candidates are ghost entries — no tx should be sent.
        let ghost_registry = GhostRegistry::new(vec![], vec![ORPHAN_A, ORPHAN_B, ORPHAN_C]);

        let tx = SharedTxManager::new();
        let driver = RegistrationDriver::new(
            MockDiscovery { instances: vec![] },
            StubProofProvider,
            ghost_registry,
            tx.clone(),
            StubSignerClient,
            default_config(CancellationToken::new()),
            None,
        )
        .expect("test driver construction succeeds");

        let registered = vec![ORPHAN_A, ORPHAN_B, ORPHAN_C];
        driver.deregister_orphans(&HashSet::new(), &registered).await.unwrap();

        assert!(tx.sent_calldata().is_empty(), "all ghosts should be skipped, no txs sent");
    }

    #[tokio::test]
    async fn discover_and_resolve_admits_recently_launched_unhealthy_to_active_and_registerable() {
        // A recently-launched Unhealthy instance must (1) be included in
        // `registerable` (the recent-launch exception in
        // `is_recently_launched_unhealthy`), and (2) contribute its
        // signer to `active_signers` (preventing premature
        // deregistration). The orphan-dereg pass over the active set
        // must NOT touch the signer even though it's already onchain.
        let addr = ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();
        let launch_time = Some(SystemTime::now() - Duration::from_secs(300));

        let instance_under_test =
            instance_with_launch_time(EP1, InstanceHealthStatus::Unhealthy, launch_time);
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);

        let tx = SharedTxManager::new();
        let driver = cycle_driver(
            vec![instance_under_test.clone()],
            signer_client,
            // addr is already onchain; without active_signers protection it would be deregistered.
            MockRegistry::with_signers(vec![addr]),
            tx.clone(),
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
        assert!(resolution.ok_to_dereg, "single reachable instance clears the majority guard");

        // Orphan-dereg pass must not deregister the signer (it's in active_signers).
        driver.run_orphan_dereg(&resolution.active_signers).await.unwrap();

        assert!(
            tx.sent_calldata().is_empty(),
            "already-registered signer should not be deregistered"
        );
    }

    // ── discover_and_resolve + run_orphan_dereg tests ──────────────────

    /// When discovery returns zero instances the active set is empty, so
    /// every onchain signer is an orphan and must be deregistered.
    /// Verifies both that `discover_and_resolve` flips `ok_to_dereg` to
    /// `true` for the legitimate zero-instance case and that
    /// `run_orphan_dereg` emits a deregistration tx per onchain signer.
    #[rstest]
    #[case::single_orphan(vec![ORPHAN_A])]
    #[case::multiple_orphans(vec![ORPHAN_A, ORPHAN_B, ORPHAN_C])]
    #[tokio::test]
    async fn run_orphan_dereg_deregisters_all_onchain_signers_when_discovery_is_empty(
        #[case] orphans: Vec<Address>,
    ) {
        let expected_count = orphans.len();
        let tx = SharedTxManager::new();
        let driver = cycle_driver(
            vec![], // no discovered instances
            MockSignerClient::from_keys(&[]),
            MockRegistry::with_signers(orphans.clone()),
            tx.clone(),
            CancellationToken::new(),
        );

        let resolution = driver.discover_and_resolve().await.unwrap();
        assert!(resolution.active_signers.is_empty(), "no instances → no active signers");
        assert!(
            resolution.ok_to_dereg,
            "zero-instance fleet drain is a legitimate empty active set",
        );

        driver.run_orphan_dereg(&resolution.active_signers).await.unwrap();

        let sent = tx.sent_calldata();
        assert_eq!(sent.len(), expected_count, "all onchain signers should be deregistered");

        // Verify each deregistration targets the correct signer.
        for orphan in orphans {
            let expected = ITEEProverRegistry::deregisterSignerCall { signer: orphan }.abi_encode();
            assert!(
                sent.iter().any(|s| s[..] == expected[..]),
                "expected deregistration of {orphan}"
            );
        }
    }

    #[tokio::test]
    async fn discover_and_resolve_majority_unreachable_clears_ok_to_dereg() {
        // 3 instances discovered, but only 1 is reachable via MockSignerClient.
        // reachable * 2 (= 2) <= total (= 3) → majority guard fires, so
        // `ok_to_dereg` must be `false` and the orphan-dereg pass would
        // be skipped by the production loop.
        let instances = vec![
            instance(EP1, InstanceHealthStatus::Healthy),
            instance(EP2, InstanceHealthStatus::Healthy),
            instance(EP3, InstanceHealthStatus::Healthy),
        ];

        // Only EP1 has a key; the other two will fail signer_public_key.
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let tx = SharedTxManager::new();
        let driver = cycle_driver(
            instances,
            signer_client,
            MockRegistry::all_registered(vec![ORPHAN_B]),
            tx.clone(),
            CancellationToken::new(),
        );

        let resolution = driver.discover_and_resolve().await.unwrap();

        assert_eq!(resolution.reachable_count, 1);
        assert_eq!(resolution.total_count, 3);
        assert!(
            !resolution.ok_to_dereg,
            "1/3 reachable: majority guard should block orphan-dereg pass",
        );
        // Resolution itself sends no onchain tx (no CRL revocation).
        assert!(tx.sent_calldata().is_empty(), "discover_and_resolve must not send txs");
    }

    #[tokio::test]
    async fn discover_and_resolve_clears_ok_to_dereg_when_cancelled_before_run() {
        // Cancellation observed by `discover_and_resolve` after the
        // resolve loop completes must drive `ok_to_dereg = false` so the
        // production caller skips `run_orphan_dereg` entirely, even
        // though the majority guard would otherwise pass.
        let instances = vec![
            instance(EP1, InstanceHealthStatus::Healthy),
            instance(EP2, InstanceHealthStatus::Healthy),
        ];

        let signer_client =
            MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)]);

        let cancel = CancellationToken::new();
        let tx = SharedTxManager::new();

        let driver = cycle_driver(
            instances,
            signer_client,
            MockRegistry::all_registered(vec![ORPHAN_C]),
            tx.clone(),
            cancel.clone(),
        );

        cancel.cancel();
        let resolution = driver.discover_and_resolve().await.unwrap();

        assert!(
            !resolution.ok_to_dereg,
            "cancellation must clear ok_to_dereg even if majority guard would pass",
        );
        // And `run_orphan_dereg` itself is cancel-aware — call it
        // directly to confirm it bails out without loading the registry.
        driver.run_orphan_dereg(&resolution.active_signers).await.unwrap();
        assert!(tx.sent_calldata().is_empty(), "no txs should be sent after cancellation");
    }

    #[tokio::test]
    async fn discover_and_resolve_admits_draining_instance_to_active_only_not_registerable() {
        // A draining instance must contribute its signer to
        // `active_signers` (protecting it from orphan-dereg) but must
        // NOT appear in `registerable`. The orphan-dereg pass over the
        // active set then must not deregister the onchain signer.
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);
        let instances = vec![instance(EP1, InstanceHealthStatus::Draining)];

        let tx = SharedTxManager::new();
        let driver = cycle_driver(
            instances,
            signer_client,
            // The derived address for HARDHAT_KEY_0 is already onchain,
            // so it should NOT be deregistered.
            MockRegistry::with_signers(vec![HARDHAT_ACCOUNT]),
            tx.clone(),
            CancellationToken::new(),
        );

        let resolution = driver.discover_and_resolve().await.unwrap();
        assert!(
            resolution.registerable.is_empty(),
            "draining instance must not be in the registerable set",
        );
        assert!(
            resolution.active_signers.contains(&HARDHAT_ACCOUNT),
            "draining instance must contribute its signer to active_signers",
        );
        assert!(resolution.ok_to_dereg, "single reachable instance clears the majority guard");

        driver.run_orphan_dereg(&resolution.active_signers).await.unwrap();

        // No registration (draining) and no deregistration (signer is active).
        assert!(tx.sent_calldata().is_empty());
    }

    // ── Reachability guard boundary tests ────────────────────────────────
    //
    // The majority guard uses instance counts (not signer counts):
    //
    //     if !instances.is_empty() && reachable_instances * 2 <= instances.len()
    //
    // These tests verify the exact boundary and surrounding values:
    //   - 1/4 reachable → 1*2 <= 4 → true  → deregistration skipped
    //   - 2/4 reachable → 2*2 <= 4 → true  → deregistration skipped
    //   - 3/4 reachable → majority clears, but unresolved instance skips deregistration
    //   - 4/4 reachable → 4*2 <= 4 → false → deregistration proceeds

    /// All 4 endpoints and corresponding private keys, indexed for
    /// dynamic slicing in the parametrized guard test.
    const ALL_ENDPOINTS: [&str; 4] = [EP1, EP2, EP3, EP4];
    const ALL_KEYS: [&[u8; 32]; 4] =
        [&HARDHAT_KEY_0, &HARDHAT_KEY_1, &HARDHAT_KEY_2, &HARDHAT_KEY_3];

    #[rstest]
    #[case::one_of_four(1, true)]
    #[case::two_of_four(2, true)]
    #[case::three_of_four(3, true)]
    #[case::four_of_four(4, false)]
    #[tokio::test]
    async fn discover_and_resolve_reachability_guard_boundary(
        #[case] reachable_count: usize,
        #[case] should_skip_deregistration: bool,
    ) {
        // All 4 instances are discovered; only `reachable_count` have keys
        // in the MockSignerClient (the rest will fail signer_public_key).
        // Verify `ok_to_dereg` remains false while any discovered instance
        // is unresolved, and that the downstream `run_orphan_dereg` pass
        // emits calldata only when every instance resolves and the majority
        // guard clears.
        let instances: Vec<_> =
            ALL_ENDPOINTS.iter().map(|ep| instance(ep, InstanceHealthStatus::Healthy)).collect();

        let keys: Vec<(&str, &[u8; 32])> = ALL_ENDPOINTS[..reachable_count]
            .iter()
            .zip(&ALL_KEYS[..reachable_count])
            .map(|(ep, key)| (*ep, *key))
            .collect();
        let signer_client = MockSignerClient::from_keys(&keys);

        let tx = SharedTxManager::new();
        let driver = cycle_driver(
            instances,
            signer_client,
            // All reachable signers already registered, so no registration txs.
            // The orphan is onchain — deregistered only if guard passes.
            MockRegistry::all_registered(vec![ORPHAN_D]),
            tx.clone(),
            CancellationToken::new(),
        );

        let resolution = driver.discover_and_resolve().await.unwrap();
        assert_eq!(resolution.reachable_count, reachable_count);
        assert_eq!(resolution.total_count, ALL_ENDPOINTS.len());
        assert_eq!(
            resolution.ok_to_dereg, !should_skip_deregistration,
            "{reachable_count}/4 reachable: ok_to_dereg mismatch"
        );

        if resolution.ok_to_dereg {
            driver.run_orphan_dereg(&resolution.active_signers).await.unwrap();
            let sent = tx.sent_calldata();
            assert_eq!(sent.len(), 1, "{reachable_count}/4 reachable: should deregister orphan");
            let expected =
                ITEEProverRegistry::deregisterSignerCall { signer: ORPHAN_D }.abi_encode();
            assert_eq!(sent[0], Bytes::from(expected));
        } else {
            // Production caller would skip run_orphan_dereg entirely.
            assert!(
                tx.sent_calldata().is_empty(),
                "{reachable_count}/4 reachable: majority guard should skip deregistration",
            );
        }
    }

    #[tokio::test]
    async fn discover_and_resolve_includes_all_reachable_when_one_instance_is_unreachable() {
        // An unreachable instance must not prevent other instances from
        // being resolved into `registerable` in the same cycle, and its
        // instance id must land in `unresolved_instance_ids` so the
        // production reconcile pass doesn't cancel any in-flight task
        // tied to it.
        let unreachable = instance(EP4, InstanceHealthStatus::Healthy);
        let reachable = [
            instance(EP1, InstanceHealthStatus::Healthy),
            instance(EP2, InstanceHealthStatus::Healthy),
            instance(EP3, InstanceHealthStatus::Healthy),
        ];
        let instances = std::iter::once(unreachable.clone())
            .chain(reachable.iter().cloned())
            .collect::<Vec<_>>();

        // EP4 has no keys → signer_public_key will error.
        let signer_client = MockSignerClient::from_keys(&[
            (EP1, &HARDHAT_KEY_0),
            (EP2, &HARDHAT_KEY_1),
            (EP3, &HARDHAT_KEY_2),
        ]);

        let tx = SharedTxManager::new();
        let driver = cycle_driver(
            instances,
            signer_client,
            // No signers registered yet → all three reachable signers are registerable.
            MockRegistry::with_signers(vec![]),
            tx.clone(),
            CancellationToken::new(),
        );

        let resolution = driver.discover_and_resolve().await.unwrap();
        assert_eq!(
            resolution.registerable.len(),
            reachable.len(),
            "all reachable instances should be registerable despite 1 unreachable",
        );
        assert_eq!(resolution.reachable_count, reachable.len());
        assert!(
            resolution.unresolved_instance_ids.contains(&unreachable.instance_id),
            "unreachable instance must be marked as unresolved so reconcile skips its cancel-pass",
        );
        assert!(
            !resolution.ok_to_dereg,
            "unresolved instance must block orphan-dereg even when reachable majority clears",
        );

        assert!(tx.sent_calldata().is_empty(), "discovery resolution must not send txs");
    }

    /// Signer client wrapper that cancels a token after returning keys.
    ///
    /// Delegates to an inner [`MockSignerClient`] for actual key/attestation
    /// data, but cancels the given [`CancellationToken`] after the first
    /// successful `signer_public_key` call. This simulates cancellation
    /// occurring mid-cycle (after instance processing begins but before
    /// orphan deregistration).
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
        // Cancellation observed during instance resolution must drive
        // `ok_to_dereg = false` so the production caller skips
        // `run_orphan_dereg` entirely. `CancellingSignerClient` cancels
        // the shared token as a side effect of `signer_public_key`,
        // simulating a shutdown signal arriving mid-cycle.
        let instances = vec![instance(EP1, InstanceHealthStatus::Healthy)];

        let cancel = CancellationToken::new();
        let tx = SharedTxManager::new();

        let signer_client = CancellingSignerClient {
            inner: MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]),
            cancel: cancel.clone(),
        };

        let driver = Arc::new(
            RegistrationDriver::new(
                MockDiscovery { instances },
                StubProofProvider,
                MockRegistry::all_registered(vec![ORPHAN_E]),
                tx.clone(),
                signer_client,
                default_config(cancel),
                None,
            )
            .expect("test driver construction succeeds"),
        );

        let resolution = driver.discover_and_resolve().await.unwrap();
        assert!(
            !resolution.ok_to_dereg,
            "cancellation observed during resolution must clear ok_to_dereg",
        );
        // run_orphan_dereg is cancel-aware — call it to confirm it bails
        // out without loading the registry or sending any tx.
        driver.run_orphan_dereg(&resolution.active_signers).await.unwrap();
        assert!(
            tx.sent_calldata().is_empty(),
            "mid-cycle cancellation should prevent any orphan deregistration",
        );
    }

    #[tokio::test]
    async fn discover_and_resolve_multi_enclave_draining_protects_all_signers_from_deregistration()
    {
        // A draining multi-enclave instance must contribute ALL of its
        // signer addresses to `active_signers`, preventing orphan
        // deregistration for each — and must not appear in
        // `registerable` (draining → registration skipped).
        let addr0 = ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();
        let addr1 = ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_1)).unwrap();

        let instances = vec![instance(EP1, InstanceHealthStatus::Draining)];
        let signer_client = MockSignerClient::multi_enclave(EP1, &[&HARDHAT_KEY_0, &HARDHAT_KEY_1]);

        let tx = SharedTxManager::new();
        let driver = cycle_driver(
            instances,
            signer_client,
            // Both signers are onchain — without active_signers protection
            // they would be deregistered as orphans.
            MockRegistry::with_signers(vec![addr0, addr1]),
            tx.clone(),
            CancellationToken::new(),
        );

        let resolution = driver.discover_and_resolve().await.unwrap();
        assert!(
            resolution.registerable.is_empty(),
            "draining instance must not appear in the registerable set",
        );
        assert!(resolution.active_signers.contains(&addr0));
        assert!(resolution.active_signers.contains(&addr1));
        assert!(resolution.ok_to_dereg);

        driver.run_orphan_dereg(&resolution.active_signers).await.unwrap();

        // No registration (draining) and no deregistration (both signers
        // are in active_signers).
        assert!(
            tx.sent_calldata().is_empty(),
            "draining multi-enclave instance should protect all signers from deregistration",
        );
    }

    #[tokio::test]
    async fn discover_and_resolve_unhealthy_instance_is_reachable_but_not_registerable() {
        // An unhealthy instance (failing ALB health checks) that is still
        // reachable by the registrar (responds to JSON-RPC) must:
        //   1. NOT be registerable (should_register = false for Unhealthy
        //      outside the recent-launch window)
        //   2. Count as reachable (increments `reachable_count`)
        //   3. Contribute its signers to `active_signers` (preventing dereg)
        //
        // This matters because "unhealthy" in ALB terms does not mean
        // the registrar can't connect — the instance may be failing
        // application-level health checks while still responding to RPC.
        let addr_unhealthy =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();
        let addr_healthy =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_1)).unwrap();

        let healthy_inst = instance(EP2, InstanceHealthStatus::Healthy);
        let instances = vec![instance(EP1, InstanceHealthStatus::Unhealthy), healthy_inst.clone()];

        // Both instances are reachable via MockSignerClient.
        let signer_client =
            MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)]);

        let tx = SharedTxManager::new();
        let driver = cycle_driver(
            instances,
            signer_client,
            // The unhealthy signer is onchain. Without active_signers protection it would be
            // deregistered.
            MockRegistry::with_signers(vec![addr_unhealthy]),
            tx.clone(),
            CancellationToken::new(),
        );

        let resolution = driver.discover_and_resolve().await.unwrap();
        assert_eq!(resolution.reachable_count, 2, "both instances respond to RPC");
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

        driver.run_orphan_dereg(&resolution.active_signers).await.unwrap();

        assert!(
            tx.sent_calldata().is_empty(),
            "orphan pass must not deregister the unhealthy signer",
        );
    }

    #[tokio::test]
    async fn discover_and_resolve_attestation_mismatch_keeps_signer_active_and_unresolved() {
        // If the registrar reaches signer_public_key successfully but
        // then cannot pair an attestation with the signer, the instance
        // is still reachable and advertising that signer. The signer
        // must remain protected from orphan deregistration, while the
        // instance is marked unresolved so any in-flight proof task is
        // preserved for a later conclusive cycle.
        let signer_addr =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();
        let inst = instance(EP1, InstanceHealthStatus::Healthy);
        let signer_client =
            MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]).with_attestations(EP1, vec![]);

        let tx = SharedTxManager::new();
        let driver = cycle_driver(
            vec![inst.clone()],
            signer_client,
            MockRegistry::with_signers(vec![signer_addr]),
            tx.clone(),
            CancellationToken::new(),
        );

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

        driver.run_orphan_dereg(&resolution.active_signers).await.unwrap();

        assert!(
            tx.sent_calldata().is_empty(),
            "orphan pass must not deregister a signer advertised by a partially resolved instance",
        );
    }

    #[tokio::test]
    async fn discover_and_resolve_attestation_error_keeps_signer_active_and_unresolved() {
        // This is the production-shaped variant of the previous test:
        // public key resolution succeeds, but the attestation RPC itself
        // fails. The signer must still be protected from orphan dereg.
        let signer_addr =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();
        let inst = instance(EP1, InstanceHealthStatus::Healthy);
        let signer_client =
            MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]).with_attestation_failure(EP1);

        let tx = SharedTxManager::new();
        let driver = cycle_driver(
            vec![inst.clone()],
            signer_client,
            MockRegistry::with_signers(vec![signer_addr]),
            tx.clone(),
            CancellationToken::new(),
        );

        let resolution = driver.discover_and_resolve().await.unwrap();

        assert!(resolution.active_signers.contains(&signer_addr));
        assert!(resolution.registerable.is_empty());
        assert!(resolution.unresolved_instance_ids.contains(&inst.instance_id));
        assert!(!resolution.ok_to_dereg, "unresolved attestation state must block orphan-dereg",);

        driver.run_orphan_dereg(&resolution.active_signers).await.unwrap();

        assert!(
            tx.sent_calldata().is_empty(),
            "attestation RPC failures must not orphan a signer whose public key resolved",
        );
    }

    #[tokio::test]
    async fn discover_and_resolve_does_not_invoke_proof_provider_so_active_set_survives_outage() {
        // Under the spawn pipeline, proof generation runs only inside
        // spawned `run_proof_task` futures — `discover_and_resolve`
        // never touches the proof provider. So a complete Boundless
        // outage cannot empty `active_signers` and cannot trigger
        // orphan-dereg of a still-running signer.
        //
        // Asserted via `FailingProofProvider`: if `discover_and_resolve`
        // ever invoked it, the resolution would error (or skip the
        // signer); instead the signer must land in `active_signers` and
        // `run_orphan_dereg` must emit no deregistration tx for the
        // onchain signer.
        let signer_addr =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();

        let instances = vec![instance(EP1, InstanceHealthStatus::Healthy)];
        let signer_client = MockSignerClient::from_keys(&[(EP1, &HARDHAT_KEY_0)]);

        let tx = SharedTxManager::new();
        let driver = Arc::new(
            RegistrationDriver::new(
                MockDiscovery { instances },
                FailingProofProvider,
                MockRegistry::with_signers(vec![signer_addr]),
                tx.clone(),
                signer_client,
                default_config(CancellationToken::new()),
                None,
            )
            .expect("test driver construction succeeds"),
        );

        let resolution = driver.discover_and_resolve().await.unwrap();
        assert!(
            resolution.active_signers.contains(&signer_addr),
            "signer must remain in active_signers even with a failing proof provider",
        );
        assert_eq!(
            resolution.registerable.len(),
            1,
            "registerable list is computed without invoking the proof provider",
        );

        driver.run_orphan_dereg(&resolution.active_signers).await.unwrap();

        // No deregistration tx (signer is in active_signers despite the
        // proof failure path being possible downstream).
        assert!(
            tx.sent_calldata().is_empty(),
            "proof-provider failures must not cause deregistration of the signer",
        );
    }

    // ── Concurrency limit test ──────────────────────────────────────────

    /// Signer client that tracks the peak number of concurrent
    /// `signer_public_key` calls. Used to verify `max_concurrency`.
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

            // Yield to give other futures a chance to enter concurrently.
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

    #[rstest]
    #[case::serial(1)]
    #[case::limited(2)]
    #[tokio::test]
    async fn discover_and_resolve_respects_max_concurrency(#[case] max_concurrency: usize) {
        // Resolve 4 instances with a limited `max_concurrency` and verify
        // the peak concurrent `signer_public_key` count observed inside
        // `discover_and_resolve`'s `buffer_unordered` loop never exceeds
        // the configured bound. All 4 must end up in `registerable`.
        let instances: Vec<_> =
            ALL_ENDPOINTS.iter().map(|ep| instance(ep, InstanceHealthStatus::Healthy)).collect();

        let keys: Vec<(&str, &[u8; 32])> =
            ALL_ENDPOINTS.iter().copied().zip(ALL_KEYS.iter().copied()).collect();
        let inner = MockSignerClient::from_keys(&keys);
        let (signer_client, peak) = ConcurrencyTrackingSignerClient::new(inner);

        let cancel = CancellationToken::new();
        let tx = SharedTxManager::new();
        let mut config = default_config(cancel);
        config.max_concurrency = max_concurrency;

        let driver = Arc::new(
            RegistrationDriver::new(
                MockDiscovery { instances },
                StubProofProvider,
                MockRegistry::with_signers(vec![]),
                tx.clone(),
                signer_client,
                config,
                None,
            )
            .expect("test driver construction succeeds"),
        );

        let resolution = driver.discover_and_resolve().await.unwrap();

        let observed_peak = peak.load(Ordering::SeqCst);
        assert!(
            observed_peak <= max_concurrency,
            "peak concurrency {observed_peak} exceeded max_concurrency {max_concurrency}",
        );
        assert_eq!(
            resolution.registerable.len(),
            ALL_ENDPOINTS.len(),
            "all 4 healthy instances should resolve into the registerable set",
        );
        // Resolution itself emits no onchain tx — the spawn pass owns registration.
        assert!(tx.sent_calldata().is_empty(), "discover_and_resolve must not send txs");
    }

    // ── cancel-aware registry await tests ──────────────────────────────
    //
    // `get_registered_signers` in `run_orphan_dereg` is wrapped in
    // `select!` against the driver cancel token so a shutdown during the
    // RPC drops the call immediately. Proof-handler-owned registry awaits
    // are covered in `proof_handler::tests`.

    /// Per-call stall registry: parks the configured method on a
    /// never-completing future. Used to assert that the `select!`
    /// wrapper in `run_orphan_dereg` short-circuits on cancel instead of
    /// blocking on the RPC.
    struct StallingRegistry {
        signers: Vec<Address>,
    }

    impl StallingRegistry {
        fn stalling_get_registered_signers(signers: Vec<Address>) -> Self {
            Self { signers }
        }
    }

    #[async_trait]
    impl RegistryClient for StallingRegistry {
        async fn is_registered(&self, _signer: Address) -> Result<bool> {
            Ok(false)
        }

        async fn get_registered_signers(&self) -> Result<Vec<Address>> {
            std::future::pending::<()>().await;
            Ok(self.signers.clone())
        }
    }

    /// Upper bound on how long a cancel must take to abort an in-flight
    /// registry RPC. Generous enough to absorb CI jitter while still
    /// failing fast on a regression (without the `select!` wrapper the
    /// test would hang until the [`tokio::time::timeout`] backstop
    /// fires, far above this bound).
    const CANCEL_ABORT_BUDGET: Duration = Duration::from_secs(1);

    /// Soft window the test sleeps between spawning the call-under-test
    /// and firing the cancel token. Long enough that the spawned future
    /// reaches its `is_registered` await point, short enough that the
    /// total test time stays well under [`CANCEL_ABORT_BUDGET`].
    const PRE_CANCEL_WARMUP: Duration = Duration::from_millis(50);

    /// `run_orphan_dereg` MUST abort promptly when `config.cancel` fires
    /// during the `get_registered_signers` RPC. Without the `select!`
    /// wrap a stalled RPC here would extend drain latency by one
    /// round-trip even though the function never reaches the
    /// per-orphan loop that has its own cancel check.
    #[tokio::test]
    async fn run_orphan_dereg_aborts_promptly_when_cancel_fires_during_registry_stall() {
        let cancel = CancellationToken::new();
        let driver = Arc::new(
            RegistrationDriver::new(
                MockDiscovery { instances: vec![] },
                StubProofProvider,
                StallingRegistry::stalling_get_registered_signers(vec![]),
                SharedTxManager::new(),
                StubSignerClient,
                default_config(cancel.clone()),
                None,
            )
            .expect("driver constructs"),
        );

        let driver_clone = Arc::clone(&driver);
        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move {
            let active: HashSet<Address> = HashSet::new();
            let start = tokio::time::Instant::now();
            let res = driver_clone.run_orphan_dereg(&active).await;
            (res, start.elapsed(), cancel_clone)
        });

        tokio::time::sleep(PRE_CANCEL_WARMUP).await;
        cancel.cancel();

        let (result, elapsed, _alive) = tokio::time::timeout(GATED_WAIT_TIMEOUT, handle)
            .await
            .expect("run_orphan_dereg must not hang past the timeout")
            .expect("spawned task must not panic");

        assert!(result.is_ok(), "cancel-induced exit must be Ok(()): {result:?}");
        assert!(
            elapsed < CANCEL_ABORT_BUDGET,
            "cancel must abort the registry stall within {CANCEL_ABORT_BUDGET:?} (took {elapsed:?})",
        );
    }

    // ── reconcile_proof_tasks tests ─────────────────────────────────────
    //
    // These unit-tests exercise the spawn / cancel decisions without
    // spinning up the run loop. They build a synthetic
    // `DiscoveryResolution` via `dr_from_kept` and a hand-rolled
    // `pending` map, then assert exactly which tasks get cancelled
    // and which get spawned.

    #[rstest]
    #[case::no_pending_spawns_all(&[], &[(EP1, &HARDHAT_KEY_0)], 1, 0)]
    #[case::pending_for_kept_spawns_nothing(&[(EP1, &HARDHAT_KEY_0)], &[(EP1, &HARDHAT_KEY_0)], 0, 0)]
    #[case::pending_for_dropped_cancels_one(&[(EP1, &HARDHAT_KEY_0)], &[], 0, 1)]
    #[case::pending_one_kept_one_dropped(
        &[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)],
        &[(EP1, &HARDHAT_KEY_0)],
        0,
        1,
    )]
    #[case::two_new_signers_two_spawns(
        &[],
        &[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)],
        2,
        0,
    )]
    #[tokio::test]
    async fn reconcile_proof_tasks_cancel_and_spawn_passes(
        #[case] pre_existing: &[(&'static str, &'static [u8; 32])],
        #[case] kept: &[(&'static str, &'static [u8; 32])],
        #[case] expected_new_spawns: usize,
        #[case] expected_cancels: usize,
    ) {
        let harness = single_healthy_harness();
        let mut tasks: JoinSet<Result<Address>> = JoinSet::new();
        let mut pending: HashMap<Address, PendingRegistration> = HashMap::new();

        // Seed the pending map by spawning placeholder tasks for the
        // pre-existing signers. These futures park on their per-task
        // cancel token so cooperative cancellation is observable.
        let mut seeded_cancels: Vec<CancellationToken> = Vec::new();
        for (_, key) in pre_existing {
            let signer = ProverClient::derive_address(&public_key_from_private(key)).unwrap();
            let task_cancel = CancellationToken::new();
            let task_cancel_inner = task_cancel.clone();
            let handle = tasks.spawn(async move {
                task_cancel_inner.cancelled().await;
                Ok(signer)
            });
            pending.insert(
                signer,
                PendingRegistration {
                    instance_id: TEST_PENDING_INSTANCE_ID.to_string(),
                    task_id: handle.id(),
                    cancel: task_cancel.clone(),
                    cancelled_by_reconcile: false,
                },
            );
            seeded_cancels.push(task_cancel);
        }

        let resolution = dr_from_kept(kept);
        let pre_spawn_count = pending.len();
        let pre_cancelled = seeded_cancels.iter().filter(|c| c.is_cancelled()).count();

        harness.driver.reconcile_proof_tasks(&resolution, &mut tasks, &mut pending);

        let post_cancelled = seeded_cancels.iter().filter(|c| c.is_cancelled()).count();
        let new_spawns = pending.len().saturating_sub(pre_spawn_count);

        assert_eq!(new_spawns, expected_new_spawns, "spawn-pass count");
        assert_eq!(post_cancelled - pre_cancelled, expected_cancels, "cancel-pass count");

        drain_test_tasks(&mut tasks, &mut pending).await;
    }

    #[tokio::test]
    async fn reconcile_proof_tasks_idempotent_when_resolution_unchanged() {
        // Running reconcile twice with the same resolution must not
        // spawn duplicate tasks or cancel an already-pending one.
        let harness = single_healthy_harness();
        let mut tasks: JoinSet<Result<Address>> = JoinSet::new();
        let mut pending: HashMap<Address, PendingRegistration> = HashMap::new();

        let resolution = dr_from_kept(&[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)]);

        harness.driver.reconcile_proof_tasks(&resolution, &mut tasks, &mut pending);
        let after_first = pending.len();
        let snapshot_ids: HashSet<_> = pending.keys().copied().collect();

        harness.driver.reconcile_proof_tasks(&resolution, &mut tasks, &mut pending);

        assert_eq!(pending.len(), after_first, "idempotent: no extra spawns");
        let after_second: HashSet<_> = pending.keys().copied().collect();
        assert_eq!(snapshot_ids, after_second, "pending signer keys unchanged across reconciles");
        for task in pending.values() {
            assert!(!task.cancel.is_cancelled(), "kept task must not be cancelled");
        }

        drain_test_tasks(&mut tasks, &mut pending).await;
    }

    /// Vanish-then-reappear: a signer cancelled in cycle N (because it
    /// dropped from `registerable`) and then re-added in cycle N+1 must
    /// spawn a fresh task in N+1 — the cancelled `pending` entry must
    /// not block the respawn. Exercises the
    /// `filter(|t| !t.cancel.is_cancelled())` on the `in_flight` build,
    /// which is what enables single-cycle rolling-deploy convergence
    /// instead of a 2-cycle (~60s at 30s poll) latency.
    ///
    /// With the address-keyed `pending` map (one entry per signer at
    /// most), the fresh spawn-pass entry OVERWRITES the stale entry in
    /// place. The stale task itself stays in the `JoinSet` until it
    /// observes its cancel and exits; `apply_join_outcome`'s
    /// `task_id`-match guard then prevents the stale task's terminal
    /// outcome from evicting the fresh entry — that guard is exercised
    /// indirectly via the `task_id` assertion below.
    #[tokio::test]
    async fn reconcile_proof_tasks_respawns_after_vanish_and_reappear() {
        let harness = single_healthy_harness();
        let mut tasks: JoinSet<Result<Address>> = JoinSet::new();
        let mut pending: HashMap<Address, PendingRegistration> = HashMap::new();

        let signer =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();

        // Cycle N: seed pending with a placeholder task for the signer.
        // The placeholder parks on its cancel token so cooperative
        // cancellation is observable without the task self-resolving.
        let stale_cancel = CancellationToken::new();
        let stale_cancel_inner = stale_cancel.clone();
        let stale_handle = tasks.spawn(async move {
            stale_cancel_inner.cancelled().await;
            Ok(signer)
        });
        let stale_task_id = stale_handle.id();
        pending.insert(
            signer,
            PendingRegistration {
                instance_id: TEST_PENDING_INSTANCE_ID.to_string(),
                task_id: stale_task_id,
                cancel: stale_cancel.clone(),
                cancelled_by_reconcile: false,
            },
        );

        // Cycle N+1: signer absent from resolution → cancel-pass fires
        // but does not reap, so the (now cancelled) entry persists in
        // `pending` keyed by `signer`.
        let empty = dr_from_kept(&[]);
        harness.driver.reconcile_proof_tasks(&empty, &mut tasks, &mut pending);
        assert!(stale_cancel.is_cancelled(), "stale task must be cancelled by reconcile");
        assert_eq!(
            pending.get(&signer).map(|p| p.task_id),
            Some(stale_task_id),
            "cancelled entry still keyed by signer until reaped",
        );
        assert_eq!(pending.len(), 1, "no fresh spawn yet (signer not registerable this cycle)");

        // Cycle N+2 (BEFORE the stale entry is reaped): signer reappears
        // → fresh spawn must happen this cycle, not deferred to N+3. The
        // fresh entry overwrites the stale one in the address-keyed map;
        // the stale task lives on in the JoinSet (parked on its cancel
        // until drain).
        let resurrected = dr_from_kept(&[(EP1, &HARDHAT_KEY_0)]);
        harness.driver.reconcile_proof_tasks(&resurrected, &mut tasks, &mut pending);

        assert_eq!(pending.len(), 1, "still exactly one entry per signer after respawn");
        let fresh = pending.get(&signer).expect("fresh entry keyed by the resurrected signer");
        assert_ne!(fresh.task_id, stale_task_id, "fresh task_id replaces the stale one");
        assert!(!fresh.cancel.is_cancelled(), "fresh task carries a live cancel token");

        drain_test_tasks(&mut tasks, &mut pending).await;
    }

    /// Inconclusive-snapshot guard: when a signer's source instance
    /// failed to resolve this cycle (its `instance_id` is recorded in
    /// `DiscoveryResolution::unresolved_instance_ids`), reconcile MUST
    /// NOT cancel that signer's in-flight proof task — the signer is
    /// absent from `registerable` only because we couldn't tell this
    /// cycle, not because we proved it's gone or ineligible. Without
    /// this guard a single transient `signer_public_key` /
    /// `signer_attestation` / CRL hiccup during a long (~70 min)
    /// Boundless proof would abandon the in-flight work.
    #[tokio::test]
    async fn reconcile_proof_tasks_preserves_task_when_instance_fails_to_resolve() {
        let harness = single_healthy_harness();
        let mut tasks: JoinSet<Result<Address>> = JoinSet::new();
        let mut pending: HashMap<Address, PendingRegistration> = HashMap::new();

        let signer =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();

        // Seed pending with a placeholder task for the signer, tagged
        // with the instance_id we'll later mark as unresolved.
        let task_cancel = CancellationToken::new();
        let task_cancel_inner = task_cancel.clone();
        let handle = tasks.spawn(async move {
            task_cancel_inner.cancelled().await;
            Ok(signer)
        });
        pending.insert(
            signer,
            PendingRegistration {
                instance_id: TEST_PENDING_INSTANCE_ID.to_string(),
                task_id: handle.id(),
                cancel: task_cancel.clone(),
                cancelled_by_reconcile: false,
            },
        );

        // Build a resolution where the signer is absent from
        // `registerable` (so `wanted` is empty) BUT the source
        // instance is flagged as unresolved this cycle.
        let mut unresolved = HashSet::new();
        unresolved.insert(TEST_PENDING_INSTANCE_ID.to_string());
        let resolution = DiscoveryResolution {
            registerable: Vec::new(),
            active_signers: HashSet::new(),
            reachable_count: 0,
            total_count: 1,
            ok_to_dereg: false,
            unresolved_instance_ids: unresolved,
        };

        harness.driver.reconcile_proof_tasks(&resolution, &mut tasks, &mut pending);

        assert!(
            !task_cancel.is_cancelled(),
            "task tied to an unresolved instance must be preserved across the cancel-pass",
        );
        assert_eq!(pending.len(), 1, "no spurious spawn or eviction this cycle");

        // Sanity contrast: same setup, but the instance is NOT
        // unresolved → cancel-pass MUST fire. Asserts the previous
        // arm's success was due to the guard, not unrelated logic.
        let resolution_conclusive = DiscoveryResolution {
            registerable: Vec::new(),
            active_signers: HashSet::new(),
            reachable_count: 1,
            total_count: 1,
            ok_to_dereg: true,
            unresolved_instance_ids: HashSet::new(),
        };
        harness.driver.reconcile_proof_tasks(&resolution_conclusive, &mut tasks, &mut pending);
        assert!(
            task_cancel.is_cancelled(),
            "with no inconclusive guard, the cancel-pass MUST fire on the same setup",
        );

        drain_test_tasks(&mut tasks, &mut pending).await;
    }

    /// Orphan-dereg companion to
    /// [`reconcile_proof_tasks_preserves_task_when_instance_fails_to_resolve`].
    ///
    /// When `resolve_instance` fails transiently for an instance whose
    /// proof task is still in-flight, the signer is **absent from**
    /// `resolution.active_signers` (no fresh evidence this cycle) but
    /// is **present in** `pending` (reconcile preserves the task via
    /// `unresolved_instance_ids`). If the preserved task succeeds and
    /// registers the signer onchain right as the orphan-dereg pass
    /// runs, the protected set assembled by
    /// [`RunDriver::protected_signers`] MUST union the two, otherwise
    /// the very next call to `deregister_orphans` would deregister the
    /// freshly-registered signer (TOCTOU race).
    #[tokio::test]
    async fn protected_signers_union_blocks_dereg_of_freshly_registered_signer() {
        let signer =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();

        // Registry reports the signer as already registered onchain —
        // the state that exists immediately after the preserved task's
        // `registerSigner` tx confirms.
        let harness = GatedRunHarness::new(
            vec![instance(EP1, InstanceHealthStatus::Healthy)],
            &[(EP1, &HARDHAT_KEY_0)],
            MockRegistry::with_signers(vec![signer]),
        );

        // Seed `pending` with an entry for the signer tied to an
        // instance that is "unresolved" this cycle. Address-keying
        // alone is what `protected_signers` consumes; the placeholder
        // task is just there so cleanup runs through the same path the
        // production loop does.
        let mut tasks: JoinSet<Result<Address>> = JoinSet::new();
        let mut pending: HashMap<Address, PendingRegistration> = HashMap::new();
        let task_cancel = CancellationToken::new();
        let task_cancel_inner = task_cancel.clone();
        let handle = tasks.spawn(async move {
            task_cancel_inner.cancelled().await;
            Ok(signer)
        });
        pending.insert(
            signer,
            PendingRegistration {
                instance_id: TEST_PENDING_INSTANCE_ID.to_string(),
                task_id: handle.id(),
                cancel: task_cancel,
                cancelled_by_reconcile: false,
            },
        );

        // `active_signers` is empty (the signer's source instance
        // failed to resolve this cycle) but `ok_to_dereg` is true so
        // the orphan pass would otherwise run unimpeded.
        let mut unresolved = HashSet::new();
        unresolved.insert(TEST_PENDING_INSTANCE_ID.to_string());
        let resolution = DiscoveryResolution {
            registerable: Vec::new(),
            active_signers: HashSet::new(),
            reachable_count: 1,
            total_count: 1,
            ok_to_dereg: true,
            unresolved_instance_ids: unresolved,
        };

        let protected = RunDriver::protected_signers(&resolution, &pending);
        assert!(
            protected.contains(&signer),
            "protected set must include in-flight signer even when absent from active_signers",
        );

        harness.driver.run_orphan_dereg(&protected).await.unwrap();

        let sent = harness.tx.sent_calldata();
        assert_eq!(
            count_deregister_calls(&sent),
            0,
            "orphan pass must NOT deregister a signer with an in-flight proof task",
        );

        drain_test_tasks(&mut tasks, &mut pending).await;
    }

    /// Sanity contrast for
    /// [`protected_signers_union_blocks_dereg_of_freshly_registered_signer`]:
    /// with `pending` empty and the same onchain state, the orphan
    /// pass MUST deregister the signer. Proves the previous test's
    /// success was due to the union, not unrelated logic.
    #[tokio::test]
    async fn protected_signers_union_does_not_shield_when_pending_empty() {
        let signer =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();
        let harness = GatedRunHarness::new(
            vec![instance(EP1, InstanceHealthStatus::Healthy)],
            &[(EP1, &HARDHAT_KEY_0)],
            MockRegistry::with_signers(vec![signer]),
        );

        let pending: HashMap<Address, PendingRegistration> = HashMap::new();
        let resolution = DiscoveryResolution {
            registerable: Vec::new(),
            active_signers: HashSet::new(),
            reachable_count: 1,
            total_count: 1,
            ok_to_dereg: true,
            unresolved_instance_ids: HashSet::new(),
        };

        let protected = RunDriver::protected_signers(&resolution, &pending);
        assert!(protected.is_empty(), "no pending → protected set is empty");

        harness.driver.run_orphan_dereg(&protected).await.unwrap();

        let sent = harness.tx.sent_calldata();
        assert_eq!(
            count_deregister_calls(&sent),
            1,
            "with no in-flight task and no active signer, orphan pass MUST deregister",
        );
    }

    // ── drain_proof_tasks metric-gating test ────────────────────────────

    /// At shutdown, [`RegistrationDriver::drain_proof_tasks`] MUST count
    /// only the tasks whose cancellation it actually drives — tasks
    /// already cancelled by a prior
    /// [`RegistrationDriver::reconcile_proof_tasks`] cancel-pass were
    /// counted at intent time and double-counting them in the drain pass
    /// would inflate the `proof_tasks_cancelled` counter. The gate uses
    /// the `cancelled_by_reconcile` flag (not
    /// `cancel.is_cancelled()`) because every per-task `signer_cancel`
    /// is a child of `DriverConfig::cancel`, so by the time drain runs
    /// the parent has already auto-cancelled every child token and
    /// `is_cancelled()` no longer distinguishes the two cases.
    ///
    /// This test wires a real prometheus recorder via
    /// [`metrics::with_local_recorder`], seeds three pending tasks (one
    /// pre-flagged as `cancelled_by_reconcile = true`, two not), drains,
    /// and asserts the counter increment equals exactly the unflagged
    /// count.
    #[cfg(feature = "metrics")]
    mod drain_metric_tests {
        use metrics_exporter_prometheus::PrometheusBuilder;

        use super::*;

        #[test]
        fn drain_counts_only_tasks_not_already_cancelled_by_reconcile() {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let recorder = PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();

            metrics::with_local_recorder(&recorder, || {
                rt.block_on(async {
                    let mut tasks: JoinSet<Result<Address>> = JoinSet::new();
                    let mut pending: HashMap<Address, PendingRegistration> = HashMap::new();

                    // Seed: (key, was_flagged_by_reconcile). The flagged
                    // task must NOT re-count at drain; the two unflagged
                    // ones must count exactly once each.
                    let seed: &[(&[u8; 32], bool)] =
                        &[(&HARDHAT_KEY_0, true), (&HARDHAT_KEY_1, false), (&HARDHAT_KEY_2, false)];

                    for (key, flagged) in seed {
                        let signer =
                            ProverClient::derive_address(&public_key_from_private(key)).unwrap();
                        let cancel = CancellationToken::new();
                        let cancel_inner = cancel.clone();
                        let handle = tasks.spawn(async move {
                            cancel_inner.cancelled().await;
                            Ok(signer)
                        });
                        pending.insert(
                            signer,
                            PendingRegistration {
                                instance_id: TEST_PENDING_INSTANCE_ID.to_string(),
                                task_id: handle.id(),
                                cancel: cancel.clone(),
                                cancelled_by_reconcile: *flagged,
                            },
                        );
                        // Simulate the reconcile cancel-pass having
                        // already fired for the flagged entry — this is
                        // the precise state drain encounters at
                        // shutdown for tasks reconcile already counted.
                        if *flagged {
                            cancel.cancel();
                        }
                    }

                    RunDriver::drain_proof_tasks(&mut tasks, &mut pending).await;
                });
            });

            let rendered = handle.render();
            assert!(
                rendered.contains("base_registrar_proof_tasks_cancelled 2"),
                "drain must count only the unflagged tasks once each (expected 2); \
                 double-count would render `3`, miscount `1`. Got:\n{rendered}",
            );
        }
    }

    // ── reconcile_proof_tasks: dedupe + indexing tests ─────────────────

    /// Driver specialisation used by the spawn-pass indexing tests so a
    /// [`RecordingProofProvider`] can capture the `(signer, attestation)`
    /// pairs handed to each spawned task.
    type RecordingDriver = RegistrationDriver<
        MockDiscovery,
        RecordingProofProvider,
        MockRegistry,
        SharedTxManager,
        MockSignerClient,
    >;

    /// Builds a driver suitable for direct `reconcile_proof_tasks`
    /// invocation: the registry reports no signers as registered (so
    /// each task reaches the proof step), and the proof provider
    /// records and exits.
    fn recording_driver(
        keys: &[(&str, &[u8; 32])],
        proof_provider: RecordingProofProvider,
    ) -> Arc<RecordingDriver> {
        Arc::new(
            RegistrationDriver::new(
                MockDiscovery { instances: vec![] },
                proof_provider,
                MockRegistry::with_signers(vec![]),
                SharedTxManager::new(),
                MockSignerClient::from_keys(keys),
                default_config(CancellationToken::new()),
                None,
            )
            .expect("recording driver constructs cleanly"),
        )
    }

    #[tokio::test]
    async fn reconcile_proof_tasks_dedupes_signer_across_registerable_entries() {
        // Two `RegisterableSigner` entries report the SAME signer
        // address (misconfig: two prover instances were provisioned
        // with identical enclave keys). The spawn pass must only
        // spawn one task — duplicating would later trigger two
        // `tx_manager.send()` calls for the same signer and waste
        // nonces.
        let proof_provider = RecordingProofProvider::default();
        let driver = recording_driver(
            &[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_0)],
            proof_provider.clone(),
        );

        // Both entries carry the same derived address but different
        // attestation bytes so an accidental second spawn would be
        // visible as a stale-attestation race in `recorded`.
        let signer =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();
        let entry_a = RegisterableSigner {
            instance: instance(EP1, InstanceHealthStatus::Healthy),
            signer,
            attestation: b"attestation-from-instance-a".to_vec(),
            enclave_index: 0,
        };
        let entry_b = RegisterableSigner {
            instance: instance(EP2, InstanceHealthStatus::Healthy),
            signer,
            attestation: b"attestation-from-instance-b".to_vec(),
            enclave_index: 0,
        };
        let resolution = DiscoveryResolution {
            registerable: vec![entry_a, entry_b],
            active_signers: HashSet::from([signer]),
            reachable_count: 2,
            total_count: 2,
            ok_to_dereg: false,
            unresolved_instance_ids: HashSet::new(),
        };

        let mut tasks: JoinSet<Result<Address>> = JoinSet::new();
        let mut pending: HashMap<Address, PendingRegistration> = HashMap::new();

        driver.reconcile_proof_tasks(&resolution, &mut tasks, &mut pending);

        assert_eq!(pending.len(), 1, "exactly one task should spawn for a duplicate signer");
        let (&only_signer, _entry) = pending.iter().next().unwrap();
        assert_eq!(only_signer, signer, "the spawned task is keyed by the deduplicated signer");

        // Let the single task run, record its attestation, and exit.
        wait_for("the lone spawned task recorded its attestation", || {
            !proof_provider.snapshot().is_empty()
        })
        .await;
        drain_test_tasks(&mut tasks, &mut pending).await;

        let snap = proof_provider.snapshot();
        assert_eq!(snap.len(), 1, "exactly one signer recorded across both entries");
    }

    #[rstest]
    #[case::forward_order(false)]
    #[case::reversed_order(true)]
    #[tokio::test]
    async fn reconcile_proof_tasks_pairs_attestation_with_signer(#[case] reverse: bool) {
        // After the flatten in `discover_and_resolve`, each
        // `RegisterableSigner` carries its own `(signer, attestation)`
        // pair, so mispairing via the old `attestations[idx]` indexing
        // bug class is structurally impossible. This test asserts the
        // spawn pass forwards each entry's `signer` and `attestation`
        // consistently — regardless of the order entries appear in the
        // registerable vector — by recording the attestation the
        // provider received per signer.
        let signer_a =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();
        let signer_b =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_1)).unwrap();
        assert_ne!(signer_a, signer_b, "test setup: distinct signer addresses");

        let att_a: Vec<u8> = b"attestation-aligned-to-A".to_vec();
        let att_b: Vec<u8> = b"attestation-aligned-to-B".to_vec();

        let entry_a = RegisterableSigner {
            instance: instance(EP1, InstanceHealthStatus::Healthy),
            signer: signer_a,
            attestation: att_a.clone(),
            enclave_index: 0,
        };
        let entry_b = RegisterableSigner {
            instance: instance(EP2, InstanceHealthStatus::Healthy),
            signer: signer_b,
            attestation: att_b.clone(),
            enclave_index: 0,
        };
        let registerable = if reverse { vec![entry_b, entry_a] } else { vec![entry_a, entry_b] };

        let proof_provider = RecordingProofProvider::default();
        let driver = recording_driver(
            &[(EP1, &HARDHAT_KEY_0), (EP2, &HARDHAT_KEY_1)],
            proof_provider.clone(),
        );

        let resolution = DiscoveryResolution {
            registerable,
            active_signers: HashSet::from([signer_a, signer_b]),
            reachable_count: 2,
            total_count: 2,
            ok_to_dereg: false,
            unresolved_instance_ids: HashSet::new(),
        };

        let mut tasks: JoinSet<Result<Address>> = JoinSet::new();
        let mut pending: HashMap<Address, PendingRegistration> = HashMap::new();

        driver.reconcile_proof_tasks(&resolution, &mut tasks, &mut pending);

        wait_for("both signers recorded their attestations", || {
            proof_provider.snapshot().len() == 2
        })
        .await;
        drain_test_tasks(&mut tasks, &mut pending).await;

        let snap = proof_provider.snapshot();
        assert_eq!(snap.get(&signer_a), Some(&att_a), "signer A got the A-aligned attestation");
        assert_eq!(snap.get(&signer_b), Some(&att_b), "signer B got the B-aligned attestation");
    }

    // ── reap_finished_tasks + apply_join_outcome tests ─────────────────

    #[rstest]
    #[case::ok_outcome(true)]
    #[case::err_outcome(false)]
    #[tokio::test]
    async fn reap_finished_tasks_drains_completed_and_evicts_pending(#[case] succeed: bool) {
        // Spawn one task that completes immediately; reap_finished_tasks
        // must remove it from `pending` regardless of inner success.
        let mut tasks: JoinSet<Result<Address>> = JoinSet::new();
        let mut pending: HashMap<Address, PendingRegistration> = HashMap::new();

        let handle = tasks.spawn(async move {
            if succeed {
                Ok(HARDHAT_ACCOUNT)
            } else {
                Err(RegistrarError::Transaction("synthetic".into()))
            }
        });
        pending.insert(
            HARDHAT_ACCOUNT,
            pending_registration_for_test(handle.id(), TEST_PENDING_INSTANCE_ID),
        );

        reap_until_pending_empty(&mut tasks, &mut pending).await;

        assert!(pending.is_empty(), "completed task must be evicted from pending");
        assert!(tasks.is_empty(), "JoinSet must drain to empty");
    }

    #[tokio::test]
    async fn reap_finished_tasks_leaves_in_flight_alone() {
        // A task that never completes must remain in `pending` after
        // `reap_finished_tasks` is called (it is non-blocking).
        let mut tasks: JoinSet<Result<Address>> = JoinSet::new();
        let mut pending: HashMap<Address, PendingRegistration> = HashMap::new();

        let cancel = CancellationToken::new();
        let cancel_inner = cancel.clone();
        let handle = tasks.spawn(async move {
            cancel_inner.cancelled().await;
            Ok(HARDHAT_ACCOUNT)
        });
        pending.insert(
            HARDHAT_ACCOUNT,
            PendingRegistration {
                instance_id: TEST_PENDING_INSTANCE_ID.to_string(),
                task_id: handle.id(),
                cancel,
                cancelled_by_reconcile: false,
            },
        );

        RunDriver::reap_finished_tasks(&mut tasks, &mut pending);

        assert_eq!(pending.len(), 1, "live task must remain in pending");

        drain_test_tasks(&mut tasks, &mut pending).await;
    }

    #[tokio::test]
    async fn reap_finished_tasks_is_noop_when_pending_is_empty() {
        // Sanity: the production loop calls `reap_finished_tasks` every
        // cycle, including cycles with no pending work. It must not
        // panic in that case.
        let mut tasks: JoinSet<Result<Address>> = JoinSet::new();
        let mut pending: HashMap<Address, PendingRegistration> = HashMap::new();

        RunDriver::reap_finished_tasks(&mut tasks, &mut pending);

        assert!(pending.is_empty(), "pending stays empty");
        assert!(tasks.is_empty(), "JoinSet stays empty");
    }

    // ── run() spawn-and-reap pipeline tests ─────────────────────────────
    //
    // End-to-end tests for the new run() loop. Each test spawns the
    // loop on the tokio runtime with a tightened poll interval, then
    // observes the spawn/reap pipeline via the GatedProofProvider
    // counters and the SharedTxManager calldata log. Cancellation is
    // used to stop the loop cleanly between assertions.

    #[rstest]
    #[case::one_instance(1)]
    #[case::two_instances(2)]
    #[case::three_instances(3)]
    #[case::four_instances(4)]
    #[tokio::test]
    async fn run_spawns_one_task_per_enclave_and_each_submits_registration(
        #[case] num_instances: usize,
    ) {
        // `num_instances` healthy instances, one enclave each, none yet
        // registered. The pipeline must spawn exactly `num_instances`
        // proof tasks; once released they each submit a single
        // registerSigner tx — independent of the fan-out width.
        let harness = multi_healthy_harness(num_instances);

        let run_handle = harness.spawn_run();

        wait_for("every proof task parked in gate", || harness.proof.in_flight() == num_instances)
            .await;

        // Release the gate so the proof tasks return and registrations submit.
        harness.proof.release_all();

        wait_for("every registerSigner tx submitted", || {
            count_register_calls(&harness.tx.sent_calldata()) == num_instances
        })
        .await;

        harness.shutdown(run_handle).await;

        assert_eq!(harness.proof.call_count(), num_instances, "exactly one proof per enclave");
    }

    #[rstest]
    // Scale-down 2 → 1: cancel either the first-discovered or the
    // last-discovered task, exhaustively.
    #[case::two_instances_drop_first(2, &[0])]
    #[case::two_instances_drop_last(2, &[1])]
    // Scale-down 3 → 1: cancel two of three (covers middle index too).
    #[case::three_instances_drop_two_keep_first(3, &[1, 2])]
    #[case::three_instances_drop_two_keep_last(3, &[0, 1])]
    #[tokio::test]
    async fn run_cancels_in_flight_tasks_when_instances_vanish_mid_proof(
        #[case] initial_count: usize,
        #[case] drop_indices: &[usize],
    ) {
        // Start with `initial_count` healthy instances; observe every
        // proof task parked. Remove the instances at `drop_indices` from
        // discovery; the next cycle must cancel their tasks cooperatively
        // (the signer_cancel token fires inside the registration manager's
        // biased select! around generate_proof_for_signer), and the cancelled
        // tasks exit Ok(()) without submitting a tx.
        let harness = multi_healthy_harness(initial_count);

        let run_handle = harness.spawn_run();

        wait_for("every proof task parked in gate", || harness.proof.in_flight() == initial_count)
            .await;

        // Drop the chosen indices from discovery.
        let drop_set: HashSet<usize> = drop_indices.iter().copied().collect();
        let kept_endpoints: Vec<&'static str> = ALL_ENDPOINTS[..initial_count]
            .iter()
            .enumerate()
            .filter_map(|(i, ep)| (!drop_set.contains(&i)).then_some(*ep))
            .collect();
        let kept_count = kept_endpoints.len();
        let dropped_count = drop_indices.len();
        harness.discovery.set(
            kept_endpoints.iter().map(|ep| instance(ep, InstanceHealthStatus::Healthy)).collect(),
        );

        // The reconcile pass on the next cycle must cancel every
        // dropped instance's task.
        wait_for("dropped proof tasks cancelled", || harness.proof.in_flight() == kept_count).await;

        // Surviving proofs are still parked; release the gate to let
        // them through.
        harness.proof.release_all();

        wait_for("every surviving instance registered", || {
            count_register_calls(&harness.tx.sent_calldata()) == kept_count
        })
        .await;

        harness.shutdown(run_handle).await;

        let sent = harness.tx.sent_calldata();
        assert_eq!(
            count_register_calls(&sent),
            kept_count,
            "{dropped_count} dropped instance(s) must NOT have submitted a registration"
        );
    }

    #[rstest]
    // The exact bug this entire refactor fixes: a long-running proof
    // for one already-discovered instance must not block discovery
    // from picking up newly-launched instances. We cover three
    // scale-up sizes so a regression that, e.g., serialises one new
    // task per N existing in-flight tasks would still be caught.
    #[case::scale_up_1_to_2(1, 2)]
    #[case::scale_up_1_to_3(1, 3)]
    #[case::scale_up_2_to_4(2, 4)]
    #[tokio::test]
    async fn run_continues_discovery_while_proof_tasks_are_in_flight(
        #[case] initial_count: usize,
        #[case] final_count: usize,
    ) {
        assert!(final_count > initial_count, "test case must scale up");
        let harness = multi_healthy_harness(initial_count);

        let run_handle = harness.spawn_run();

        wait_for("initial proof tasks parked in gate", || {
            harness.proof.in_flight() == initial_count
        })
        .await;
        let initial_call_count = harness.proof.call_count();

        // Now scale up: add the new instances while the initial ones'
        // proofs are still parked in the gate.
        harness.discovery.set(
            ALL_ENDPOINTS[..final_count]
                .iter()
                .map(|ep| instance(ep, InstanceHealthStatus::Healthy))
                .collect(),
        );

        // Every new instance must enter the proof pipeline.
        wait_for("all proof tasks spawned despite existing in-flight tasks", || {
            harness.proof.in_flight() == final_count
        })
        .await;
        let added = final_count - initial_count;
        assert_eq!(
            harness.proof.call_count() - initial_call_count,
            added,
            "newly-discovered instances must each have generated a proof"
        );

        // Release everything and let them register.
        harness.proof.release_all();
        wait_for("every instance registered", || {
            count_register_calls(&harness.tx.sent_calldata()) == final_count
        })
        .await;

        harness.shutdown(run_handle).await;
    }

    /// Reasons a healthy-looking input still produces no proof and no
    /// registration tx, used to parametrize
    /// [`run_does_not_register_when`].
    #[derive(Debug, Clone, Copy)]
    enum NoRegisterReason {
        /// Instance is `Draining` — `should_register()` is `false`.
        InstanceDraining,
        /// Instance is `Unhealthy` with no recent launch time —
        /// `should_register()` is `false` and the unhealthy-grace
        /// window doesn't apply.
        InstanceUnhealthy,
        /// Instance is `Healthy` but the derived signer is already on the
        /// onchain registry, so registration short-circuits in `is_registered()`.
        SignerAlreadyRegistered,
    }

    /// Builds the (instance-list, registry) pair for each
    /// [`NoRegisterReason`] case.
    fn build_no_register_inputs(reason: NoRegisterReason) -> (Vec<ProverInstance>, MockRegistry) {
        match reason {
            NoRegisterReason::InstanceDraining => {
                (vec![instance(EP1, InstanceHealthStatus::Draining)], empty_registry())
            }
            NoRegisterReason::InstanceUnhealthy => {
                (vec![instance(EP1, InstanceHealthStatus::Unhealthy)], empty_registry())
            }
            NoRegisterReason::SignerAlreadyRegistered => {
                let signer =
                    ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();
                (
                    vec![instance(EP1, InstanceHealthStatus::Healthy)],
                    MockRegistry::all_registered(vec![signer]),
                )
            }
        }
    }

    #[rstest]
    #[case::instance_draining(NoRegisterReason::InstanceDraining)]
    #[case::instance_unhealthy_no_launch_time(NoRegisterReason::InstanceUnhealthy)]
    #[case::signer_already_registered(NoRegisterReason::SignerAlreadyRegistered)]
    #[tokio::test]
    async fn run_does_not_register_when(#[case] reason: NoRegisterReason) {
        // For every reason listed above, the pipeline must observe the
        // instance (otherwise this test would pass trivially with a
        // broken discovery loop) and then choose not to spawn a proof
        // / not to submit a registration. The two `assert_eq!(0, …)`s
        // are the same invariant — only the *reason* the input failed
        // to register differs.
        let (initial_instances, registry) = build_no_register_inputs(reason);
        let harness = GatedRunHarness::new(initial_instances, &[(EP1, &HARDHAT_KEY_0)], registry);

        let run_handle = harness.spawn_run();

        // Let multiple cycles elapse so we can be confident the loop
        // observed the instance and chose not to register.
        tokio::time::sleep(GATED_POLL_INTERVAL * MIN_CYCLES_IN_OBSERVATION_WINDOW as u32).await;

        assert_eq!(
            harness.proof.call_count(),
            0,
            "generate_proof must not be called for reason: {reason:?}"
        );
        assert_eq!(
            count_register_calls(&harness.tx.sent_calldata()),
            0,
            "no registration tx must be submitted for reason: {reason:?}"
        );

        harness.shutdown(run_handle).await;
    }

    #[tokio::test]
    async fn run_deregisters_orphan_signer_via_orphan_pass() {
        // No discovered instances → active_signers is empty →
        // every onchain signer is an orphan. ok_to_dereg is true
        // when total_count == 0, so the orphan pass fires and
        // deregisters ORPHAN_A.
        let harness = GatedRunHarness::new(vec![], &[], MockRegistry::with_signers(vec![ORPHAN_A]));

        let run_handle = harness.spawn_run();

        wait_for("ORPHAN_A deregistered", || {
            count_deregister_calls(&harness.tx.sent_calldata()) == 1
        })
        .await;

        harness.shutdown(run_handle).await;

        assert_eq!(harness.proof.call_count(), 0, "no proofs needed for orphan-only cycle");
    }

    #[tokio::test]
    async fn run_drains_pending_proof_tasks_on_shutdown() {
        // When the cancel token fires while a proof task is still
        // parked in the gate, the shutdown path must cancel + abort
        // the JoinSet and the task must terminate cleanly (Ok via the
        // signer_cancel select! branch).
        let harness = single_healthy_harness();

        let run_handle = harness.spawn_run();

        wait_for("EP1 proof parked", || harness.proof.in_flight() == 1).await;

        // Don't release the gate — let shutdown handle the cleanup.
        harness.shutdown(run_handle).await;

        let sent = harness.tx.sent_calldata();
        assert_eq!(count_register_calls(&sent), 0, "no registration submitted at shutdown");
    }

    // ── apply_join_outcome + run() additional coverage ──────────────────
    //
    // These cover three gaps left after the initial spawn-and-reap test
    // suite: the panic arm of [`apply_join_outcome`], the proof-failure
    // path through the registration manager, and a single cycle that
    // fires both the registration and orphan-dereg passes.

    #[tokio::test]
    async fn apply_join_outcome_drops_pending_entry_when_task_panics() {
        // The `Err(JoinError)` arm of `apply_join_outcome` must still
        // remove the panicked task from `pending`. With the address-
        // keyed map and the panic path losing the task's return value,
        // the recovery routes through `find_signer_by_task_id` (the
        // O(n) scan over `pending`) to map `JoinError::id()` back to
        // the signer address. The per-task cancel handle is dropped
        // and the proof-task-completed metric still fires. The full
        // reap path is exercised so this is also a coverage test for
        // `reap_finished_tasks` routing the `JoinError` correctly.
        let mut tasks: JoinSet<Result<Address>> = JoinSet::new();
        let mut pending: HashMap<Address, PendingRegistration> = HashMap::new();

        let handle = tasks.spawn(async {
            panic!("synthetic proof-task panic for apply_join_outcome test");
        });
        pending.insert(
            HARDHAT_ACCOUNT,
            pending_registration_for_test(handle.id(), TEST_PENDING_INSTANCE_ID),
        );

        reap_until_pending_empty(&mut tasks, &mut pending).await;

        assert!(pending.is_empty(), "panicked task must be evicted from pending");
        assert!(tasks.is_empty(), "JoinSet must drain to empty");
    }

    /// Address-keyed cleanup safety: a stale task whose `pending` entry
    /// was overwritten by a same-cycle respawn for the same signer
    /// must NOT evict the fresh entry when its terminal `Ok(signer)`
    /// flows through [`RegistrationDriver::apply_join_outcome`]. The
    /// `task_id`-match guard in the success arm is the protection — a
    /// stale completion without the guard would leak the fresh task
    /// from `pending` (orphaning its cancel handle) and corrupt
    /// shutdown bookkeeping.
    #[tokio::test]
    async fn apply_join_outcome_preserves_fresh_entry_when_stale_task_completes_for_same_signer() {
        let mut tasks: JoinSet<Result<Address>> = JoinSet::new();
        let mut pending: HashMap<Address, PendingRegistration> = HashMap::new();

        let signer = HARDHAT_ACCOUNT;

        // Stale task: spawn one that returns immediately. Capture its
        // task_id but DO NOT insert it into `pending` keyed by signer —
        // instead simulate the post-overwrite state where the stale
        // entry is gone but its outcome is still in-flight on the
        // JoinSet.
        let stale_handle = tasks.spawn(async move { Ok(signer) });
        let stale_task_id = stale_handle.id();

        // Fresh task: spawn another, register it under `signer` in
        // `pending`. This entry must survive the stale task's
        // terminal outcome.
        let fresh_cancel = CancellationToken::new();
        let fresh_cancel_inner = fresh_cancel.clone();
        let fresh_handle = tasks.spawn(async move {
            fresh_cancel_inner.cancelled().await;
            Ok(signer)
        });
        let fresh_task_id = fresh_handle.id();
        assert_ne!(stale_task_id, fresh_task_id, "test setup: distinct task ids");

        pending.insert(
            signer,
            PendingRegistration {
                instance_id: TEST_PENDING_INSTANCE_ID.to_string(),
                task_id: fresh_task_id,
                cancel: fresh_cancel.clone(),
                cancelled_by_reconcile: false,
            },
        );

        // Drain just the stale task; the fresh one parks on its
        // cancel token so reap only sees the stale outcome.
        let started = std::time::Instant::now();
        loop {
            if let Some(joined) = tasks.try_join_next_with_id() {
                RunDriver::apply_join_outcome(Some(joined), &mut pending);
                break;
            }
            if started.elapsed() > GATED_WAIT_TIMEOUT {
                panic!("stale task never resolved");
            }
            tokio::time::sleep(REAP_POLL_INTERVAL).await;
        }

        assert_eq!(pending.len(), 1, "fresh entry must NOT be evicted by stale completion");
        let entry = pending.get(&signer).expect("fresh entry still keyed by signer");
        assert_eq!(entry.task_id, fresh_task_id, "fresh task_id preserved");
        assert!(!entry.cancel.is_cancelled(), "fresh cancel handle untouched");

        // Tear down: cancel the fresh task and drain.
        drain_test_tasks(&mut tasks, &mut pending).await;
    }

    /// Mirror of the success-arm fresh/stale test for the
    /// [`RegistrationDriver::apply_join_outcome`] inner-`Err` arm: a
    /// stale task failing must NOT evict the fresh entry that
    /// reconcile dropped into the slot for the same signer. The
    /// [`RegistrationDriver::remove_if_task_matches`] guard threaded
    /// through all three arms is what enforces this — without it,
    /// `find_signer_by_task_id` returning `None` for the stale id is
    /// the only thing preventing fresh-entry eviction, which is a
    /// fragile implicit invariant.
    #[tokio::test]
    async fn apply_join_outcome_err_arm_preserves_fresh_entry_when_stale_task_fails_for_same_signer()
     {
        let mut tasks: JoinSet<Result<Address>> = JoinSet::new();
        let mut pending: HashMap<Address, PendingRegistration> = HashMap::new();

        let signer = HARDHAT_ACCOUNT;

        // Stale task returns an immediate `Err`. Its task_id is NOT in
        // `pending` — simulating the post-overwrite state.
        let stale_handle = tasks.spawn(async move {
            Err(RegistrarError::Config("synthetic stale proof failure".to_string()))
        });
        let stale_task_id = stale_handle.id();

        // Fresh task parked on its cancel token; pending keys it under
        // `signer`. This entry must survive the stale `Err` outcome.
        let fresh_cancel = CancellationToken::new();
        let fresh_cancel_inner = fresh_cancel.clone();
        let fresh_handle = tasks.spawn(async move {
            fresh_cancel_inner.cancelled().await;
            Ok(signer)
        });
        let fresh_task_id = fresh_handle.id();
        assert_ne!(stale_task_id, fresh_task_id, "test setup: distinct task ids");

        pending.insert(
            signer,
            PendingRegistration {
                instance_id: TEST_PENDING_INSTANCE_ID.to_string(),
                task_id: fresh_task_id,
                cancel: fresh_cancel.clone(),
                cancelled_by_reconcile: false,
            },
        );

        // Drain just the stale task; the fresh one parks on its
        // cancel token so reap only sees the stale outcome.
        let started = std::time::Instant::now();
        loop {
            if let Some(joined) = tasks.try_join_next_with_id() {
                RunDriver::apply_join_outcome(Some(joined), &mut pending);
                break;
            }
            if started.elapsed() > GATED_WAIT_TIMEOUT {
                panic!("stale task never resolved");
            }
            tokio::time::sleep(REAP_POLL_INTERVAL).await;
        }

        assert_eq!(pending.len(), 1, "fresh entry must NOT be evicted by stale Err");
        let entry = pending.get(&signer).expect("fresh entry still keyed by signer");
        assert_eq!(entry.task_id, fresh_task_id, "fresh task_id preserved");
        assert!(!entry.cancel.is_cancelled(), "fresh cancel handle untouched");

        drain_test_tasks(&mut tasks, &mut pending).await;
    }

    #[tokio::test]
    async fn run_isolates_proof_failure_and_continues_pipeline_for_other_signers() {
        // EP1's proof errors out; EP2's proof must still complete and
        // submit a registration. This proves:
        //   1. The failing task does not park the gate (EP1 errors
        //      before the await in `generate_proof`), so the loop sees
        //      exactly one in-flight task.
        //   2. EP2's registration lands — the failing task did not
        //      block the pipeline serially behind it.
        //   3. The failed task is evicted from `pending`, so shutdown
        //      drains cleanly within [`GATED_WAIT_TIMEOUT`].
        //
        // The failing signer's proof returns `Err` before the
        // `tx_manager.send()` call in the registration manager, so it can never
        // produce a `registerSigner` calldata frame — regardless of
        // how many cycles re-spawn it under the static `MockRegistry`.
        // Every entry in `count_register_calls` is therefore attributable
        // to the surviving signer.
        let harness = multi_healthy_harness(2);
        let failing_signer =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0)).unwrap();
        let surviving_signer =
            ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_1)).unwrap();
        assert_ne!(failing_signer, surviving_signer, "test setup: distinct signer addresses");
        harness.proof.fail_for_signers([failing_signer]);

        let run_handle = harness.spawn_run();

        // Only the surviving signer's task should park in the gate; the failing
        // task errors immediately and returns from registration without awaiting.
        wait_for("exactly one proof parked (failing signer errored before the gate)", || {
            harness.proof.in_flight() == 1
        })
        .await;

        // Release the gate so the surviving signer can register.
        harness.proof.release_all();
        wait_for("surviving signer registered while failing signer errored", || {
            count_register_calls(&harness.tx.sent_calldata()) >= 1
        })
        .await;

        harness.shutdown(run_handle).await;

        // Final state: at least one registration landed, the gate
        // observed exactly one parked task (asserted above), and no
        // tasks are still in flight after shutdown.
        assert_eq!(harness.proof.in_flight(), 0, "every parked proof returned by shutdown");
    }

    #[tokio::test]
    async fn run_handles_orphan_dereg_and_active_registration_in_same_cycle() {
        // Mixed-mode cycle: EP1 is healthy + unregistered (so it must
        // register), while ORPHAN_A is already onchain but has no
        // backing instance (so it must be deregistered). Both passes
        // must run in the same cycle and both transactions must land.
        let harness = GatedRunHarness::new(
            vec![instance(EP1, InstanceHealthStatus::Healthy)],
            &[(EP1, &HARDHAT_KEY_0)],
            MockRegistry::with_signers(vec![ORPHAN_A]),
        );

        let run_handle = harness.spawn_run();

        // Orphan dereg runs in the foreground each cycle; it must
        // submit immediately without waiting for the proof gate.
        wait_for("ORPHAN_A deregistered", || {
            count_deregister_calls(&harness.tx.sent_calldata()) == 1
        })
        .await;

        // EP1's proof is parked in the gate; releasing it must let
        // the registration through alongside the already-completed
        // dereg.
        wait_for("EP1 proof parked", || harness.proof.in_flight() == 1).await;
        harness.proof.release_all();
        wait_for("EP1 registered", || count_register_calls(&harness.tx.sent_calldata()) == 1).await;

        harness.shutdown(run_handle).await;

        let sent = harness.tx.sent_calldata();
        assert_eq!(count_register_calls(&sent), 1, "EP1 registration submitted exactly once");
        assert_eq!(
            count_deregister_calls(&sent),
            1,
            "ORPHAN_A deregistration submitted exactly once"
        );
    }

    /// `unhealthy_registration_window` parametric test: an `Unhealthy`
    /// instance whose `launch_time` falls *inside* the window must register via
    /// the full `run()` pipeline; one whose `launch_time` falls *outside* the
    /// window, or who has no `launch_time` at all, must not register.
    #[rstest]
    // Recently-launched unhealthy instance: should register.
    #[case::recent_launch_registers(
        Some(Duration::from_secs(60 * 10)),
        true,
    )]
    // Old unhealthy instance well past the window: should NOT register.
    #[case::old_launch_does_not_register(
        Some(Duration::from_secs(60 * 60 * 24)),
        false,
    )]
    // Unhealthy instance with no launch_time: cannot age-gate, so
    // defaults to the safe path and does NOT register.
    #[case::missing_launch_does_not_register(None, false)]
    #[tokio::test]
    async fn run_registers_unhealthy_only_within_grace_window(
        #[case] age_below_now: Option<Duration>,
        #[case] expect_registration: bool,
    ) {
        let launch_time = age_below_now.map(|age| SystemTime::now() - age);
        let inst = instance_with_launch_time(EP1, InstanceHealthStatus::Unhealthy, launch_time);
        let harness = GatedRunHarness::new(vec![inst], &[(EP1, &HARDHAT_KEY_0)], empty_registry());

        let run_handle = harness.spawn_run();

        if expect_registration {
            // Eligible instance: a proof task must park on the gate,
            // then the registration must land once we release.
            wait_for("eligible unhealthy proof parked in gate", || harness.proof.in_flight() == 1)
                .await;
            harness.proof.release_all();
            wait_for("unhealthy-within-window signer registered", || {
                count_register_calls(&harness.tx.sent_calldata()) >= 1
            })
            .await;
        } else {
            // Ineligible instance: give the loop at least a couple of
            // cycles so a faulty short-circuit would have time to
            // spawn a proof task and submit a tx. The gate stays
            // unreleased — if anything parked we'd never reach the
            // shutdown timeout below.
            tokio::time::sleep(GATED_POLL_INTERVAL * MIN_CYCLES_IN_OBSERVATION_WINDOW as u32).await;
            assert_eq!(
                harness.proof.in_flight(),
                0,
                "ineligible unhealthy instance must not spawn a proof task"
            );
            assert!(
                count_register_calls(&harness.tx.sent_calldata()) == 0,
                "ineligible unhealthy instance must not register"
            );
        }

        harness.shutdown(run_handle).await;
    }

    // NOTE on real-data fixtures: the pipeline tests above intentionally
    // use the [`GatedProofProvider`] (which synthesises empty
    // attestation-proof bytes) and the [`MockSignerClient`]'s default
    // `b"mock-attestation"` byte string. The end-to-end run loop never
    // parses these blobs in this test configuration: CRL pre-checks are
    // disabled in [`default_config`], and `MockRegistry` does not verify
    // calldata. The canonical 4-certificate chain from
    // `crate::test_utils` is exercised separately and exhaustively by
    // the cert manager tests, which target the actual cert-parsing code
    // paths. Mixing real cert bytes into these orchestration tests would
    // not exercise any additional code and would add ~3 KB of attestation
    // byte literals to every test run.
}
