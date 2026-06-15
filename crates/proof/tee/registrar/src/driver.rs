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
    use alloy_primitives::{Address, B256, Bloom, Bytes, address};
    use alloy_rpc_types_eth::TransactionReceipt;
    use alloy_sol_types::SolCall;
    use async_trait::async_trait;
    use base_proof_contracts::ITEEProverRegistry;
    use base_proof_tee_nitro_attestation_prover::{AttestationProof, AttestationProofProvider};
    use base_tx_manager::{SendHandle, TxCandidate, TxManager};
    use hex_literal::hex;
    use k256::ecdsa::SigningKey;
    use rstest::rstest;
    use tokio_util::sync::CancellationToken;
    use url::Url;

    use super::*;
    use crate::{
        InstanceHealthStatus, NitroVerifierClient, RegistryClient, Result, SignerClient,
        SignerManager,
    };

    // ── Shared constants ────────────────────────────────────────────────

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

    /// Mock Nitro verifier used to satisfy the driver's required certificate
    /// manager dependency in tests that do not exercise CRL checks.
    #[derive(Debug)]
    struct MockNitroVerifier;

    #[async_trait]
    impl NitroVerifierClient for MockNitroVerifier {
        fn address(&self) -> Address {
            Address::ZERO
        }

        async fn is_revoked(&self, _cert_hash: B256) -> Result<bool> {
            Ok(false)
        }
    }

    fn mock_nitro_verifier() -> Arc<dyn NitroVerifierClient> {
        Arc::new(MockNitroVerifier)
    }

    fn mock_cert_manager(
        config: &DriverConfig,
        tx_manager: SharedTxManager,
    ) -> CertManager<SharedTxManager> {
        CertManager::new(&config.crl, mock_nitro_verifier(), tx_manager)
            .expect("test cert manager builds")
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

    /// Driver-level signer lifecycle mock. It records calls without running
    /// proof generation or registry cleanup, keeping run-loop tests focused on
    /// driver orchestration.
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

    // ── Driver constructors ─────────────────────────────────────────────

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

    type CycleDriver = RegistrationDriver<
        MockDiscovery,
        MockSignerClient,
        Arc<SignerManager<StubProofProvider, MockRegistry, SharedTxManager>>,
        SharedTxManager,
    >;

    /// Builds a fully-configured driver for primitive-level tests that
    /// invoke `discover_and_resolve` and `run_orphan_dereg` directly
    /// (rather than the spawn pipeline in `run`). Returns an `Arc` so tests
    /// that spawn the run loop can keep handles for state inspection.
    fn cycle_driver(
        instances: Vec<ProverInstance>,
        signer_client: MockSignerClient,
        registry: MockRegistry,
        tx: SharedTxManager,
        cancel: CancellationToken,
    ) -> Arc<CycleDriver> {
        let config = default_config(cancel);
        let signer_manager = Arc::new(SignerManager::new(
            StubProofProvider,
            registry,
            tx.clone(),
            config.signer_manager.clone(),
        ));
        let cert_manager = mock_cert_manager(&config, tx);
        Arc::new(RegistrationDriver::new(
            MockDiscovery { instances },
            signer_client,
            config,
            cert_manager,
            signer_manager,
        ))
    }

    #[test]
    fn new_accepts_injected_cert_manager() {
        let mut config = default_config(CancellationToken::new());
        config.crl.enabled = true;
        let tx = SharedTxManager::new();
        let signer_manager = Arc::new(SignerManager::new(
            StubProofProvider,
            MockRegistry::with_signers(vec![]),
            tx.clone(),
            config.signer_manager.clone(),
        ));
        let cert_manager = mock_cert_manager(&config, tx);

        RegistrationDriver::new(
            MockDiscovery { instances: vec![] },
            MockSignerClient::from_keys(&[]),
            config,
            cert_manager,
            signer_manager,
        );
    }
    // ── Pipeline test infrastructure ────────────────────────────────────
    //
    // Used by top-level `run` tests. Designed so the entire run loop can be
    // driven from a `tokio::test` without real sleeps: the gated proof provider
    // parks every spawned proof on a `CancellationToken` that the test releases
    // when it has observed the behaviour it cares about.

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
        MockSignerClient,
        Arc<SignerManager<GatedProofProvider, MockRegistry, SharedTxManager>>,
        SharedTxManager,
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

        /// Spawns the registration loop on the current runtime, returning
        /// the `JoinHandle` so the test can await shutdown.
        fn spawn_run(&self) -> tokio::task::JoinHandle<Result<()>> {
            let driver = Arc::clone(&self.driver);
            tokio::spawn(async move { driver.run_loop().await })
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

    #[tokio::test]
    async fn run_loop_uses_injected_signer_manager_boundary() {
        let cancel = CancellationToken::new();
        let mut config = default_config(cancel.clone());
        config.poll_interval = GATED_POLL_INTERVAL;

        let tx = SharedTxManager::new();
        let signer_manager = MockSignerManager::new(cancel.clone());
        let signer_manager_handle = signer_manager.clone();
        let signer = ProverClient::derive_address(&public_key_from_private(&HARDHAT_KEY_0))
            .expect("test key derives");
        let cert_manager = mock_cert_manager(&config, tx);

        let driver = RegistrationDriver::<
            MockDiscovery,
            MockSignerClient,
            MockSignerManager,
            SharedTxManager,
        >::new(
            MockDiscovery { instances: vec![instance(EP1, InstanceHealthStatus::Healthy)] },
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
        driver
            .signer_manager
            .run_orphan_dereg(&resolution.active_signers, &driver.config.cancel)
            .await
            .unwrap();

        assert!(
            tx.sent_calldata().is_empty(),
            "already-registered signer should not be deregistered"
        );
    }

    // ── discover_and_resolve + run_orphan_dereg tests ──────────────────

    #[tokio::test]
    async fn discover_and_resolve_allows_orphan_pass_when_discovery_is_empty() {
        let driver = cycle_driver(
            vec![],
            MockSignerClient::from_keys(&[]),
            MockRegistry::with_signers(vec![ORPHAN_A, ORPHAN_B, ORPHAN_C]),
            SharedTxManager::new(),
            CancellationToken::new(),
        );

        let resolution = driver.discover_and_resolve().await.unwrap();
        assert!(resolution.active_signers.is_empty(), "no instances → no active signers");
        assert!(
            resolution.ok_to_dereg,
            "zero-instance fleet drain is a legitimate empty active set",
        );
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
        driver
            .signer_manager
            .run_orphan_dereg(&resolution.active_signers, &driver.config.cancel)
            .await
            .unwrap();
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

        driver
            .signer_manager
            .run_orphan_dereg(&resolution.active_signers, &driver.config.cancel)
            .await
            .unwrap();

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
            driver
                .signer_manager
                .run_orphan_dereg(&resolution.active_signers, &driver.config.cancel)
                .await
                .unwrap();
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
        let config = default_config(cancel);
        let signer_manager = Arc::new(SignerManager::new(
            StubProofProvider,
            MockRegistry::all_registered(vec![ORPHAN_E]),
            tx.clone(),
            config.signer_manager.clone(),
        ));
        let cert_manager = mock_cert_manager(&config, tx.clone());

        let driver = Arc::new(RegistrationDriver::new(
            MockDiscovery { instances },
            signer_client,
            config,
            cert_manager,
            signer_manager,
        ));

        let resolution = driver.discover_and_resolve().await.unwrap();
        assert!(
            !resolution.ok_to_dereg,
            "cancellation observed during resolution must clear ok_to_dereg",
        );
        // run_orphan_dereg is cancel-aware — call it to confirm it bails
        // out without loading the registry or sending any tx.
        driver
            .signer_manager
            .run_orphan_dereg(&resolution.active_signers, &driver.config.cancel)
            .await
            .unwrap();
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

        driver
            .signer_manager
            .run_orphan_dereg(&resolution.active_signers, &driver.config.cancel)
            .await
            .unwrap();

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

        driver
            .signer_manager
            .run_orphan_dereg(&resolution.active_signers, &driver.config.cancel)
            .await
            .unwrap();

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

        driver
            .signer_manager
            .run_orphan_dereg(&resolution.active_signers, &driver.config.cancel)
            .await
            .unwrap();

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

        driver
            .signer_manager
            .run_orphan_dereg(&resolution.active_signers, &driver.config.cancel)
            .await
            .unwrap();

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
        let config = default_config(CancellationToken::new());
        let signer_manager = Arc::new(SignerManager::new(
            FailingProofProvider,
            MockRegistry::with_signers(vec![signer_addr]),
            tx.clone(),
            config.signer_manager.clone(),
        ));
        let cert_manager = mock_cert_manager(&config, tx.clone());
        let driver = Arc::new(RegistrationDriver::new(
            MockDiscovery { instances },
            signer_client,
            config,
            cert_manager,
            signer_manager,
        ));

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

        driver
            .signer_manager
            .run_orphan_dereg(&resolution.active_signers, &driver.config.cancel)
            .await
            .unwrap();

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
        config.signer_manager.max_concurrency = max_concurrency;
        let signer_manager = Arc::new(SignerManager::new(
            StubProofProvider,
            MockRegistry::with_signers(vec![]),
            tx.clone(),
            config.signer_manager.clone(),
        ));
        let cert_manager = mock_cert_manager(&config, tx.clone());

        let driver = Arc::new(RegistrationDriver::new(
            MockDiscovery { instances },
            signer_client,
            config,
            cert_manager,
            signer_manager,
        ));

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

    // ── run() additional coverage ─────────────────────────────────────

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
