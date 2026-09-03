//! Signer lifecycle orchestration for the registrar.
//!
//! Coordinates hinted certificate caching, signer registration, and orphaned
//! signer cleanup after the driver has resolved discovered prover instances.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{Address, B256, Bytes, b256, keccak256};
use alloy_sol_types::SolCall;
use base_proof_contracts::{
    CertManagerAuthorizationError, CertManagerClient, ContractError, ITEEProverRegistry,
    TEEProverRegistryClient, decode_cert_manager_authorization_error,
    encode_register_signer_calldata, encode_revoke_cert_calldata,
    encode_verify_ca_cert_with_hints_calldata, encode_verify_client_cert_with_hints_calldata,
};
use base_tx_manager::{TxCandidate, TxManager, TxManagerError};
use tokio::{
    sync::{Mutex as AsyncMutex, Semaphore},
    task::{self, JoinError, JoinSet},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use crate::{
    AttestationPlanner, CertKind, CertPlan, DiscoveryResolution, P384Hints, PINNED_ROOT_CERT_HASH,
    RegistrarError, RegistrarMetrics, RegistrationHints, RegistrationPlan, Result, crl,
};

/// Default maximum number of transaction submission retries for transient
/// errors before giving up.
pub const DEFAULT_MAX_TX_RETRIES: u32 = 3;

/// Default initial delay between transaction submission retries in seconds.
pub const DEFAULT_TX_RETRY_DELAY_SECS: u64 = 5;

const ATTESTATION_NONCE_DOMAIN: &[u8] = b"base-proof-tee-registrar:attestation-nonce:v1";
const CRL_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TX_RETRY_BACKOFF_DELAY: Duration = Duration::from_secs(60);
const DEBUG_MODE_PCR0_HASH: B256 =
    b256!("0xc980e59163ce244bb4bb6211f48c7b46f88a4f40943e84eb99bdc41e129bd293");

/// Runtime configuration for [`SignerManager`].
#[derive(Debug, Clone, Copy)]
pub struct SignerManagerConfig {
    /// `TEEProverRegistry` contract address.
    pub registry_address: Address,
    /// Maximum concurrent hint-generation tasks.
    pub max_concurrency: usize,
    /// Maximum number of transaction submission retries for transient errors.
    pub max_tx_retries: u32,
    /// Delay between transaction submission retries.
    pub tx_retry_delay: Duration,
    /// Maximum attestation age accepted before onchain submission.
    pub max_attestation_age: Duration,
    /// Whether AWS CRL checks and issuer/serial revocation transactions are enabled.
    pub crl_checks_enabled: bool,
}

/// What one certificate-cache transaction attempt did onchain.
///
/// Each variant maps to exactly one terminal `cert_cache_tx_total` outcome, so the retry loop can
/// record the counter from this value instead of at each of its exits.
#[derive(Debug)]
pub enum CacheTxAttempt {
    /// The submitted transaction cached a usable certificate.
    Cached,
    /// The certificate is cached, but not by this attempt's successful receipt.
    ObservedCached,
    /// The signer task was cancelled before the onchain state could be confirmed.
    Cancelled,
    /// The transaction was included but reverted.
    Reverted(RegistrarError),
    /// The attempt failed permanently.
    Failed(RegistrarError),
    /// The submission failed with a retryable error and retries remain.
    Retry(TxManagerError),
}

impl CacheTxAttempt {
    /// Returns the bounded `cert_cache_tx_total` outcome label for this attempt.
    pub const fn outcome(&self) -> &'static str {
        match self {
            Self::Cached => RegistrarMetrics::TX_OUTCOME_SUCCEEDED,
            Self::ObservedCached => RegistrarMetrics::TX_OUTCOME_OBSERVED_CACHED,
            Self::Cancelled => RegistrarMetrics::TX_OUTCOME_CANCELLED,
            Self::Reverted(_) => RegistrarMetrics::TX_OUTCOME_REVERTED,
            Self::Failed(_) => RegistrarMetrics::TX_OUTCOME_FAILED,
            Self::Retry(_) => RegistrarMetrics::TX_OUTCOME_RETRY,
        }
    }
}

/// State for a registration task currently in flight.
///
/// One entry per signer address. The pending map is keyed by [`Address`] so
/// each signer has at most one active registration task.
#[derive(Debug)]
pub struct PendingRegistration {
    /// Originating instance ID, used to preserve tasks when the source
    /// instance is unresolved and to attribute log lines.
    pub instance_id: String,
    /// `JoinSet` task id for this registration task.
    pub task_id: task::Id,
    /// Cooperative cancel handle for this single task.
    pub cancel: CancellationToken,
}

/// Coordinates signer registration and orphan signer deregistration.
#[derive(Debug)]
pub struct SignerManager<R, C, T> {
    registry: R,
    cert_manager: C,
    tx_manager: T,
    hint_semaphore: Arc<Semaphore>,
    cert_locks: Mutex<HashMap<B256, Weak<AsyncMutex<()>>>>,
    crl_http_client: Option<reqwest::Client>,
    registry_address: Address,
    max_tx_retries: u32,
    tx_retry_delay: Duration,
    max_attestation_age: Duration,
}

impl<R, C, T> SignerManager<R, C, T> {
    /// Creates a signer manager from the signer lifecycle dependencies.
    pub fn new(
        registry: R,
        cert_manager: C,
        tx_manager: T,
        config: SignerManagerConfig,
    ) -> Result<Self> {
        let crl_http_client = config
            .crl_checks_enabled
            .then(|| {
                reqwest::Client::builder()
                    .timeout(CRL_FETCH_TIMEOUT)
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
            })
            .transpose()
            .map_err(|e| RegistrarError::Config(format!("failed to build CRL HTTP client: {e}")))?;
        Ok(Self {
            registry,
            cert_manager,
            tx_manager,
            hint_semaphore: Arc::new(Semaphore::new(config.max_concurrency.max(1))),
            cert_locks: Mutex::new(HashMap::new()),
            crl_http_client,
            registry_address: config.registry_address,
            max_tx_retries: config.max_tx_retries,
            tx_retry_delay: config.tx_retry_delay,
            max_attestation_age: config.max_attestation_age,
        })
    }

    /// Derives the deterministic attestation nonce for a signer.
    pub fn attestation_nonce(&self, signer: Address) -> [u8; 32] {
        Self::attestation_nonce_for(self.registry_address, signer)
    }

    /// Derives the deterministic attestation nonce for a registry/signer pair.
    pub fn attestation_nonce_for(registry_address: Address, signer: Address) -> [u8; 32] {
        let mut input = Vec::with_capacity(
            ATTESTATION_NONCE_DOMAIN.len()
                + registry_address.as_slice().len()
                + signer.as_slice().len(),
        );
        input.extend_from_slice(ATTESTATION_NONCE_DOMAIN);
        input.extend_from_slice(registry_address.as_slice());
        input.extend_from_slice(signer.as_slice());
        *keccak256(input)
    }

    fn cert_lock(&self, cert_hash: B256) -> Arc<AsyncMutex<()>> {
        let mut locks = self.cert_locks.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&cert_hash).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(cert_hash, Arc::downgrade(&lock));
        lock
    }

    fn retry_delay(&self, retry: u32) -> Duration {
        self.tx_retry_delay
            .saturating_mul(2_u32.saturating_pow(retry.saturating_sub(1)))
            .min(MAX_TX_RETRY_BACKOFF_DELAY.max(self.tx_retry_delay))
    }

    fn now() -> (u64, u64) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        (now.as_secs(), u64::try_from(now.as_millis()).unwrap_or(u64::MAX))
    }

    fn validate_plan(&self, expected_signer: Address, plan: &RegistrationPlan) -> Result<()> {
        if plan.signer != expected_signer {
            return Err(RegistrarError::InvalidAttestationProof(format!(
                "signer mismatch: expected {expected_signer}, got {}",
                plan.signer
            )));
        }
        let expected_nonce = self.attestation_nonce(expected_signer);
        if plan.nonce.as_deref() != Some(expected_nonce.as_slice()) {
            return Err(RegistrarError::InvalidAttestationProof(format!(
                "nonce mismatch for signer {expected_signer}: expected 0x{}, got {}",
                hex::encode(expected_nonce),
                plan.nonce
                    .as_deref()
                    .map(|nonce| format!("0x{}", hex::encode(nonce)))
                    .unwrap_or_else(|| "null".into())
            )));
        }
        if plan.pcr0.len() != 48 || keccak256(&plan.pcr0) == DEBUG_MODE_PCR0_HASH {
            return Err(RegistrarError::InvalidAttestationProof(
                "PCR0 must be a non-debug 48-byte measurement".into(),
            ));
        }
        if plan.root_cert_hash != PINNED_ROOT_CERT_HASH
            || keccak256(&plan.root_cert) != PINNED_ROOT_CERT_HASH
        {
            return Err(RegistrarError::InvalidAttestationProof(
                "registration plan does not use the pinned Nitro root certificate".into(),
            ));
        }
        if plan.certs.is_empty() || plan.certs.last().is_none_or(|cert| cert.kind != CertKind::Leaf)
        {
            return Err(RegistrarError::InvalidAttestationProof(
                "certificate plan must end in one leaf certificate".into(),
            ));
        }
        if plan.certs[..plan.certs.len() - 1].iter().any(|cert| cert.kind != CertKind::Ca) {
            return Err(RegistrarError::InvalidAttestationProof(
                "non-leaf certificate plan contains a non-CA certificate".into(),
            ));
        }
        let mut parent_hash = plan.root_cert_hash;
        for cert in &plan.certs {
            if cert.parent_cert_hash != parent_hash {
                return Err(RegistrarError::InvalidAttestationProof(format!(
                    "certificate {} has an unexpected parent hash",
                    cert.label
                )));
            }
            parent_hash = cert.cert_hash;
        }
        if plan.leaf_cert_hash != parent_hash {
            return Err(RegistrarError::InvalidAttestationProof(
                "leaf certificate hash does not match the final cache step".into(),
            ));
        }
        self.ensure_attestation_fresh(expected_signer, plan.timestamp)
    }

    fn ensure_attestation_fresh(&self, signer: Address, timestamp_ms: u64) -> Result<()> {
        let (now_secs, now_ms) = Self::now();
        let timestamp_secs = timestamp_ms / 1000;
        // A same-second attestation is valid to submit because transaction execution will occur
        // in a later block; the Registry remains the authority on its block-timestamp boundary.
        if timestamp_secs > now_secs {
            return Err(RegistrarError::FutureAttestationProof { signer, timestamp_ms });
        }
        if timestamp_secs.saturating_add(self.max_attestation_age.as_secs()) <= now_secs {
            let age = Duration::from_millis(now_ms.saturating_sub(timestamp_ms));
            warn!(
                signer = %signer,
                age_secs = age.as_secs(),
                max_age_secs = self.max_attestation_age.as_secs(),
                timestamp_ms,
                "pre-submission freshness check failed"
            );
            return Err(RegistrarError::StaleAttestationProof {
                signer,
                age,
                max_age: self.max_attestation_age,
            });
        }
        Ok(())
    }
}

/// Driver-owned set of in-flight registration tasks.
#[derive(Debug, Default)]
pub struct ProofTaskSet {
    tasks: JoinSet<(Address, Result<()>)>,
    /// Pending registration tasks keyed by signer address.
    pub pending: HashMap<Address, PendingRegistration>,
}

impl ProofTaskSet {
    /// Drains every task that has already finished from `tasks`.
    pub fn reap_finished_tasks(&mut self) {
        while let Some(joined) = self.tasks.try_join_next_with_id() {
            self.apply_join_outcome(joined);
        }
    }

    /// Consumes one `JoinSet` outcome and updates `pending` plus metrics.
    fn apply_join_outcome(
        &mut self,
        joined: std::result::Result<(task::Id, (Address, Result<()>)), JoinError>,
    ) {
        RegistrarMetrics::proof_tasks_completed().increment(1);
        match joined {
            Ok((id, (signer, result))) => {
                let removed = self.pending.remove(&signer);
                let was_cancelled =
                    removed.as_ref().is_some_and(|entry| entry.cancel.is_cancelled());
                let instance_id =
                    removed.as_ref().map_or("missing", |entry| entry.instance_id.as_str());
                match result {
                    Ok(()) => {
                        RegistrarMetrics::record_proof_task_completed(if was_cancelled {
                            RegistrarMetrics::PROOF_TASK_OUTCOME_CANCELLED
                        } else {
                            RegistrarMetrics::PROOF_TASK_OUTCOME_SUCCEEDED
                        });
                        debug!(
                            task_id = ?id,
                            signer = %signer,
                            instance = %instance_id,
                            pending_entry_found = removed.is_some(),
                            "registration task completed"
                        );
                    }
                    Err(e) => {
                        RegistrarMetrics::record_proof_task_completed(
                            RegistrarMetrics::PROOF_TASK_OUTCOME_FAILED,
                        );
                        warn!(
                            task_id = ?id,
                            error = %e,
                            signer = %signer,
                            instance = %instance_id,
                            pending_entry_found = removed.is_some(),
                            "registration task failed"
                        );
                        RegistrarMetrics::processing_errors_total().increment(1);
                    }
                }
            }
            Err(join_err) => {
                RegistrarMetrics::record_proof_task_completed(
                    RegistrarMetrics::PROOF_TASK_OUTCOME_JOIN_ERROR,
                );
                let id = join_err.id();
                let removed = self.pending.extract_if(|_, pending| pending.task_id == id).next();
                warn!(
                    task_id = ?id,
                    error = %join_err,
                    signer = ?removed.as_ref().map(|(signer, _)| *signer),
                    instance = ?removed.as_ref().map(|(_, task)| task.instance_id.as_str()),
                    pending_entry_found = removed.is_some(),
                    "registration task join error"
                );
                RegistrarMetrics::processing_errors_total().increment(1);
            }
        }
    }

    /// Cancels every pending task cooperatively and awaits natural completion.
    pub async fn drain_proof_tasks(&mut self) {
        for task in self.pending.values_mut() {
            if !task.cancel.is_cancelled() {
                RegistrarMetrics::proof_tasks_cancelled().increment(1);
                task.cancel.cancel();
            }
        }
        while let Some(joined) = self.tasks.join_next_with_id().await {
            self.apply_join_outcome(joined);
        }
        RegistrarMetrics::proof_tasks_pending().set(0.0);
    }
}

impl<R, C, T> SignerManager<R, C, T>
where
    R: TEEProverRegistryClient,
    C: CertManagerClient,
    T: TxManager,
{
    /// Attempts to register a signer onchain if it is not already registered.
    #[instrument(
        name = "registrar.register_signer",
        skip_all,
        fields(instance_id = %instance_id, signer = %signer_address),
        err(Display)
    )]
    pub async fn register_signer(
        &self,
        instance_id: &str,
        signer_address: Address,
        attestation_bytes: &[u8],
        signer_cancel: &CancellationToken,
    ) -> Result<()> {
        let plan = AttestationPlanner::prepare_registration_plan(attestation_bytes)?;
        self.register_plan(instance_id, signer_address, plan, None, signer_cancel).await
    }

    async fn register_plan(
        &self,
        instance_id: &str,
        signer_address: Address,
        plan: RegistrationPlan,
        prepared_hints: Option<RegistrationHints>,
        signer_cancel: &CancellationToken,
    ) -> Result<()> {
        let result = self
            .register_plan_attempt(instance_id, signer_address, plan, prepared_hints, signer_cancel)
            .await;
        // `proof_stale` counts registration attempts abandoned because the attestation aged out.
        // The freshness check itself also runs on the CRL revocation path, which swallows its
        // error once per revoked certificate, so classify the terminal error here instead.
        if matches!(result, Err(RegistrarError::StaleAttestationProof { .. })) {
            RegistrarMetrics::record_registration_stage(
                RegistrarMetrics::REGISTRATION_STAGE_PROOF_STALE,
            );
        }
        result
    }

    async fn register_plan_attempt(
        &self,
        instance_id: &str,
        signer_address: Address,
        plan: RegistrationPlan,
        prepared_hints: Option<RegistrationHints>,
        signer_cancel: &CancellationToken,
    ) -> Result<()> {
        if self.registry.address() != self.registry_address {
            return Err(RegistrarError::Config(format!(
                "registry client address {} does not match configured address {}",
                self.registry.address(),
                self.registry_address
            )));
        }
        self.validate_plan(signer_address, &plan)?;
        if signer_cancel.is_cancelled() {
            return Ok(());
        }

        // Keep revocation monitoring active for already-registered signers. Persisting a newly
        // observed CRL entry protects every future registration that shares this certificate chain.
        if !self.check_revocations(&plan, signer_cancel).await? {
            return Ok(());
        }
        if !self.check_crls(&plan, signer_cancel).await? {
            return Ok(());
        }

        let Some(already_registered) = signer_cancel
            .run_until_cancelled(self.registry.is_registered_signer(signer_address))
            .await
            .transpose()?
        else {
            return Ok(());
        };
        if already_registered {
            RegistrarMetrics::record_registration_stage(
                RegistrarMetrics::REGISTRATION_STAGE_ALREADY_REGISTERED,
            );
            debug!(signer = %signer_address, instance = %instance_id, "already registered");
            return Ok(());
        }

        if !self.validate_root_cache(&plan, signer_cancel).await? {
            return Ok(());
        }

        let hints = match prepared_hints {
            Some(hints) => hints,
            None => {
                RegistrarMetrics::record_registration_stage(
                    RegistrarMetrics::REGISTRATION_STAGE_PROOF_STARTED,
                );
                let Some(permit) = signer_cancel
                    .run_until_cancelled(Arc::clone(&self.hint_semaphore).acquire_owned())
                    .await
                else {
                    RegistrarMetrics::record_registration_stage(
                        RegistrarMetrics::REGISTRATION_STAGE_PROOF_CANCELLED,
                    );
                    return Ok(());
                };
                let permit = permit.map_err(|_| {
                    RegistrarMetrics::record_registration_stage(
                        RegistrarMetrics::REGISTRATION_STAGE_PROOF_FAILED,
                    );
                    RegistrarError::Service("hint-generation semaphore closed unexpectedly".into())
                })?;
                let hint_plan = plan.clone();
                let task = task::spawn_blocking(move || {
                    let _permit = permit;
                    P384Hints::for_registration_plan(&hint_plan.root_cert, &hint_plan)
                });
                let Some(hints) = signer_cancel.run_until_cancelled(task).await else {
                    RegistrarMetrics::record_registration_stage(
                        RegistrarMetrics::REGISTRATION_STAGE_PROOF_CANCELLED,
                    );
                    return Ok(());
                };
                let hints = hints
                    .map_err(|e| {
                        RegistrarMetrics::record_registration_stage(
                            RegistrarMetrics::REGISTRATION_STAGE_PROOF_FAILED,
                        );
                        RegistrarError::Service(format!("hint-generation task failed: {e}"))
                    })?
                    .map_err(|e| {
                        RegistrarMetrics::record_registration_stage(
                            RegistrarMetrics::REGISTRATION_STAGE_PROOF_FAILED,
                        );
                        crate::PlannerError::from(e)
                    })?;
                RegistrarMetrics::record_registration_stage(
                    RegistrarMetrics::REGISTRATION_STAGE_PROOF_SUCCEEDED,
                );
                hints
            }
        };

        if hints.cert_signature_hints.len() != plan.certs.len() {
            return Err(RegistrarError::InvalidAttestationProof(format!(
                "certificate hint count mismatch: expected {}, got {}",
                plan.certs.len(),
                hints.cert_signature_hints.len()
            )));
        }
        for (cert, cert_hints) in plan.certs.iter().zip(&hints.cert_signature_hints) {
            RegistrarMetrics::record_hint_size(
                RegistrarMetrics::cert_kind_label(cert.kind),
                cert_hints.len(),
            );
        }
        RegistrarMetrics::record_hint_size(
            RegistrarMetrics::HINT_KIND_ATTESTATION,
            hints.attestation_hints.len(),
        );
        for (cert, cert_hints) in plan.certs.iter().zip(&hints.cert_signature_hints) {
            if !self
                .ensure_cert_cached(cert, cert_hints, signer_address, plan.timestamp, signer_cancel)
                .await?
            {
                return Ok(());
            }
        }

        self.submit_registration(instance_id, signer_address, &plan, &hints, signer_cancel).await
    }

    async fn check_revocations(
        &self,
        plan: &RegistrationPlan,
        signer_cancel: &CancellationToken,
    ) -> Result<bool> {
        let checks = std::iter::once(("pinned root certificate", plan.root_cert_hash))
            .chain(plan.certs.iter().map(|cert| (cert.label.as_str(), cert.revocation_id)));
        for (label, cert_id) in checks {
            RegistrarMetrics::onchain_revocation_checks_total().increment(1);
            let Some(revoked) = signer_cancel
                .run_until_cancelled(self.cert_manager.is_revoked(cert_id))
                .await
                .transpose()?
            else {
                return Ok(false);
            };
            if revoked {
                RegistrarMetrics::onchain_revocations_detected().increment(1);
                return Err(RegistrarError::RevokedCertificate { label: label.into(), cert_id });
            }
        }
        Ok(true)
    }

    async fn validate_root_cache(
        &self,
        plan: &RegistrationPlan,
        signer_cancel: &CancellationToken,
    ) -> Result<bool> {
        let root = CertPlan {
            kind: CertKind::Ca,
            label: "pinned root certificate".into(),
            cert: plan.root_cert.clone(),
            cert_hash: plan.root_cert_hash,
            parent_cert_hash: B256::ZERO,
            revocation_id: plan.root_cert_hash,
        };
        match self.validate_cached_cert(&root, signer_cancel).await? {
            None => Ok(false),
            Some(true) => Ok(true),
            Some(false) => Err(RegistrarError::InvalidAttestationProof(
                "pinned root certificate is not cached by CertManager".into(),
            )),
        }
    }

    async fn check_crls(
        &self,
        plan: &RegistrationPlan,
        signer_cancel: &CancellationToken,
    ) -> Result<bool> {
        let Some(http_client) = &self.crl_http_client else {
            return Ok(true);
        };
        let cert_infos = crl::CertCrlInfo::from_cert_plans(&plan.certs)?;
        RegistrarMetrics::crl_checks_total().increment(1);
        let Some(revoked) = signer_cancel
            .run_until_cancelled(crl::check_chain_against_crls(&cert_infos, http_client))
            .await
        else {
            return Ok(false);
        };
        if revoked.is_empty() {
            return Ok(true);
        }
        RegistrarMetrics::crl_revocations_detected().increment(revoked.len() as u64);
        for cert in &revoked {
            if let Err(e) = self
                .submit_revocation(cert.revocation_id, plan.signer, plan.timestamp, signer_cancel)
                .await
            {
                warn!(
                    error = %e,
                    cert_id = %cert.revocation_id,
                    "failed to persist CRL revocation"
                );
                RegistrarMetrics::revoke_cert_tx_failures().increment(1);
            }
        }
        let first = revoked[0];
        Err(RegistrarError::RevokedCertificate {
            label: format!("CA certificate {}", first.index),
            cert_id: first.revocation_id,
        })
    }

    async fn submit_revocation(
        &self,
        cert_id: B256,
        signer: Address,
        timestamp_ms: u64,
        signer_cancel: &CancellationToken,
    ) -> Result<()> {
        let candidate = TxCandidate {
            tx_data: encode_revoke_cert_calldata(cert_id),
            to: Some(self.cert_manager.address()),
            ..Default::default()
        };
        for retry in 0..=self.max_tx_retries {
            if signer_cancel.is_cancelled() {
                return Ok(());
            }
            self.ensure_attestation_fresh(signer, timestamp_ms)?;
            let result = self.tx_manager.send(candidate.clone()).await;
            let observed_revoked = signer_cancel
                .run_until_cancelled(self.cert_manager.is_revoked(cert_id))
                .await
                .transpose();
            if matches!(observed_revoked, Ok(Some(true))) {
                RegistrarMetrics::revoke_cert_success_total().increment(1);
                return Ok(());
            }
            match result {
                Ok(receipt) if receipt.inner.status() => {
                    return Err(RegistrarError::InvalidAttestationProof(format!(
                        "revokeCert transaction {} succeeded without setting revocation state",
                        receipt.transaction_hash
                    )));
                }
                Ok(receipt) => {
                    RegistrarMetrics::revoke_cert_reverted_total().increment(1);
                    return Err(RegistrarError::ReceiptReverted {
                        tx_hash: receipt.transaction_hash,
                    });
                }
                Err(error) => {
                    if Self::cert_manager_authorization_error(&error).is_some() {
                        warn!(
                            error = %error,
                            cert_id = %cert_id,
                            "CertManager revocation sender is not authorized"
                        );
                    }
                    if !error.is_retryable() || retry == self.max_tx_retries {
                        return Err(error.into());
                    }
                    let retry = retry + 1;
                    if !self
                        .sleep_before_retry(retry, signer, "revocation", &error, signer_cancel)
                        .await
                    {
                        return Ok(());
                    }
                }
            }
        }
        unreachable!("bounded revocation retry loop must return")
    }

    fn cert_manager_authorization_error(
        error: &TxManagerError,
    ) -> Option<CertManagerAuthorizationError> {
        let TxManagerError::ExecutionReverted { data, reason } = error else {
            return None;
        };
        data.as_deref()
            .and_then(|data| decode_cert_manager_authorization_error(data.as_ref()))
            .or_else(|| {
                let reason = reason.as_deref()?;
                if reason.contains("NotOwner") {
                    Some(CertManagerAuthorizationError::NotOwner)
                } else if reason.contains("NotRevoker") {
                    Some(CertManagerAuthorizationError::NotRevoker)
                } else {
                    None
                }
            })
    }

    async fn ensure_cert_cached(
        &self,
        cert: &CertPlan,
        signature_hints: &[u8],
        signer: Address,
        timestamp_ms: u64,
        signer_cancel: &CancellationToken,
    ) -> Result<bool> {
        let lock = self.cert_lock(cert.cert_hash);
        let Some(_guard) = signer_cancel.run_until_cancelled(lock.lock()).await else {
            return Ok(false);
        };

        match self.validate_cached_cert(cert, signer_cancel).await? {
            None => return Ok(false),
            Some(true) => {
                RegistrarMetrics::record_cache_lookup(cert.kind, true);
                return Ok(true);
            }
            Some(false) => RegistrarMetrics::record_cache_lookup(cert.kind, false),
        }

        let tx_data = match cert.kind {
            CertKind::Ca => encode_verify_ca_cert_with_hints_calldata(
                Bytes::copy_from_slice(&cert.cert),
                cert.parent_cert_hash,
                Bytes::copy_from_slice(signature_hints),
            ),
            CertKind::Leaf => encode_verify_client_cert_with_hints_calldata(
                Bytes::copy_from_slice(&cert.cert),
                cert.parent_cert_hash,
                Bytes::copy_from_slice(signature_hints),
            ),
        };
        let candidate =
            TxCandidate { tx_data, to: Some(self.cert_manager.address()), ..Default::default() };

        for retry in 0..=self.max_tx_retries {
            if signer_cancel.is_cancelled() {
                return Ok(false);
            }
            self.ensure_attestation_fresh(signer, timestamp_ms)?;
            RegistrarMetrics::record_cache_tx(cert.kind, RegistrarMetrics::TX_OUTCOME_SUBMITTED);
            let attempt = self.cache_tx_attempt(cert, &candidate, retry, signer_cancel).await;
            RegistrarMetrics::record_cache_tx(cert.kind, attempt.outcome());
            match attempt {
                CacheTxAttempt::Cached | CacheTxAttempt::ObservedCached => return Ok(true),
                CacheTxAttempt::Cancelled => return Ok(false),
                CacheTxAttempt::Reverted(error) | CacheTxAttempt::Failed(error) => {
                    return Err(error);
                }
                CacheTxAttempt::Retry(error) => {
                    if !self
                        .sleep_before_retry(
                            retry + 1,
                            signer,
                            "certificate cache",
                            &error,
                            signer_cancel,
                        )
                        .await
                    {
                        return Ok(false);
                    }
                }
            }
        }
        unreachable!("bounded certificate retry loop must return")
    }

    /// Sends one certificate-cache transaction and classifies what it did onchain.
    ///
    /// The outcome is returned rather than recorded here so that every exit of the retry loop
    /// pairs its `submitted` count with exactly one terminal `cert_cache_tx_total` outcome.
    async fn cache_tx_attempt(
        &self,
        cert: &CertPlan,
        candidate: &TxCandidate,
        retry: u32,
        signer_cancel: &CancellationToken,
    ) -> CacheTxAttempt {
        let result = self.tx_manager.send(candidate.clone()).await;
        let state_error = match self.validate_cached_cert(cert, signer_cancel).await {
            Ok(Some(true)) => {
                return if result.as_ref().is_ok_and(|receipt| receipt.inner.status()) {
                    CacheTxAttempt::Cached
                } else {
                    CacheTxAttempt::ObservedCached
                };
            }
            Ok(None) => return CacheTxAttempt::Cancelled,
            Err(
                error @ (RegistrarError::ExpiredCertificate { .. }
                | RegistrarError::RevokedCertificate { .. }
                | RegistrarError::InvalidAttestationProof(_)),
            ) => return CacheTxAttempt::Failed(error),
            Err(error) => {
                warn!(
                    error = %error,
                    cert_hash = %cert.cert_hash,
                    "failed to reread certificate state after transaction"
                );
                Some(error)
            }
            Ok(Some(false)) => None,
        };

        match result {
            Ok(receipt) if receipt.inner.status() => {
                CacheTxAttempt::Failed(state_error.unwrap_or_else(|| {
                    RegistrarError::InvalidAttestationProof(format!(
                        "certificate {} was not usable after successful cache transaction {}",
                        cert.label, receipt.transaction_hash
                    ))
                }))
            }
            Ok(receipt) => CacheTxAttempt::Reverted(RegistrarError::CertificateCacheReverted {
                cert_hash: cert.cert_hash,
                tx_hash: receipt.transaction_hash,
            }),
            Err(error) if error.is_retryable() && retry < self.max_tx_retries => {
                CacheTxAttempt::Retry(error)
            }
            Err(error) => CacheTxAttempt::Failed(error.into()),
        }
    }

    async fn validate_cached_cert(
        &self,
        cert: &CertPlan,
        signer_cancel: &CancellationToken,
    ) -> Result<Option<bool>> {
        let Some(verified) = signer_cancel
            .run_until_cancelled(self.cert_manager.load_verified(cert.cert_hash))
            .await
            .transpose()?
        else {
            return Ok(None);
        };
        let Some(revoked) = signer_cancel
            .run_until_cancelled(self.cert_manager.is_revoked(cert.revocation_id))
            .await
            .transpose()?
        else {
            return Ok(None);
        };
        if revoked {
            return Err(RegistrarError::RevokedCertificate {
                label: cert.label.clone(),
                cert_id: cert.revocation_id,
            });
        }
        if verified.public_key.is_empty() {
            return Ok(Some(false));
        }
        let (now_secs, _) = Self::now();
        if verified.not_after < now_secs {
            return Err(RegistrarError::ExpiredCertificate {
                label: cert.label.clone(),
                not_after: verified.not_after,
            });
        }
        if verified.ca != (cert.kind == CertKind::Ca) {
            return Err(RegistrarError::InvalidAttestationProof(format!(
                "cached certificate {} has the wrong certificate kind",
                cert.label
            )));
        }

        let warm_result = match cert.kind {
            CertKind::Ca => {
                let Some(result) = signer_cancel
                    .run_until_cancelled(self.cert_manager.verify_ca_cert_with_hints(
                        Bytes::copy_from_slice(&cert.cert),
                        cert.parent_cert_hash,
                        Bytes::new(),
                    ))
                    .await
                else {
                    return Ok(None);
                };
                result.map(|returned_hash| returned_hash == cert.cert_hash)
            }
            CertKind::Leaf => {
                let Some(result) = signer_cancel
                    .run_until_cancelled(self.cert_manager.verify_client_cert_with_hints(
                        Bytes::copy_from_slice(&cert.cert),
                        cert.parent_cert_hash,
                        Bytes::new(),
                    ))
                    .await
                else {
                    return Ok(None);
                };
                result.map(|returned| !returned.ca && !returned.public_key.is_empty())
            }
        };
        match warm_result {
            Ok(true) => Ok(Some(true)),
            Ok(false) => Err(RegistrarError::InvalidAttestationProof(format!(
                "cached certificate {} returned inconsistent metadata",
                cert.label
            ))),
            Err(error)
                if error.is_execution_revert()
                    || matches!(&error, ContractError::Validation(_)) =>
            {
                Err(RegistrarError::InvalidAttestationProof(format!(
                    "cached certificate {} failed warm validation: {error}",
                    cert.label
                )))
            }
            Err(error) => Err(error.into()),
        }
    }

    #[instrument(
        name = "registrar.submit_registration",
        skip_all,
        fields(instance_id = %instance_id, signer = %signer)
    )]
    async fn submit_registration(
        &self,
        instance_id: &str,
        signer: Address,
        plan: &RegistrationPlan,
        hints: &RegistrationHints,
        signer_cancel: &CancellationToken,
    ) -> Result<()> {
        let candidate = TxCandidate {
            tx_data: encode_register_signer_calldata(
                Bytes::copy_from_slice(&plan.attestation_tbs),
                Bytes::copy_from_slice(&plan.signature),
                Bytes::copy_from_slice(&hints.attestation_hints),
            ),
            to: Some(self.registry_address),
            ..Default::default()
        };
        for retry in 0..=self.max_tx_retries {
            if signer_cancel.is_cancelled() {
                return Ok(());
            }
            self.ensure_attestation_fresh(signer, plan.timestamp)?;
            let Some(registered) = signer_cancel
                .run_until_cancelled(self.registry.is_registered_signer(signer))
                .await
                .transpose()?
            else {
                return Ok(());
            };
            if registered {
                RegistrarMetrics::record_registration_stage(
                    RegistrarMetrics::REGISTRATION_STAGE_TX_OBSERVED_REGISTERED,
                );
                return Ok(());
            }

            info!(
                signer = %signer,
                instance = %instance_id,
                registry = %self.registry_address,
                "sending hinted registration transaction"
            );
            RegistrarMetrics::record_registration_stage(
                RegistrarMetrics::REGISTRATION_STAGE_TX_SUBMITTED,
            );
            let result = self.tx_manager.send(candidate.clone()).await;
            match result {
                Ok(receipt) if receipt.inner.status() => {
                    info!(signer = %signer, tx_hash = %receipt.transaction_hash, "signer registered");
                    RegistrarMetrics::record_registration_stage(
                        RegistrarMetrics::REGISTRATION_STAGE_TX_SUCCEEDED,
                    );
                    RegistrarMetrics::registrations_total().increment(1);
                    return Ok(());
                }
                Ok(receipt) => {
                    match self.observed_registered(signer, signer_cancel).await {
                        Some(true) | None => return Ok(()),
                        Some(false) => {}
                    }
                    RegistrarMetrics::record_registration_stage(
                        RegistrarMetrics::REGISTRATION_STAGE_TX_REVERTED,
                    );
                    return Err(RegistrarError::ReceiptReverted {
                        tx_hash: receipt.transaction_hash,
                    });
                }
                Err(error) => {
                    match self.observed_registered(signer, signer_cancel).await {
                        Some(true) => {
                            info!(
                                signer = %signer,
                                error = %error,
                                "registration transaction errored but signer is registered"
                            );
                            return Ok(());
                        }
                        None => return Ok(()),
                        Some(false) => {}
                    }
                    if !error.is_retryable() || retry == self.max_tx_retries {
                        RegistrarMetrics::record_registration_stage(
                            RegistrarMetrics::REGISTRATION_STAGE_TX_FAILED,
                        );
                        return Err(error.into());
                    }
                    let retry = retry + 1;
                    RegistrarMetrics::record_registration_stage(
                        RegistrarMetrics::REGISTRATION_STAGE_TX_RETRY,
                    );
                    if !self
                        .sleep_before_retry(retry, signer, "registration", &error, signer_cancel)
                        .await
                    {
                        return Ok(());
                    }
                }
            }
        }
        unreachable!("bounded registration retry loop must return")
    }

    async fn observed_registered(
        &self,
        signer: Address,
        signer_cancel: &CancellationToken,
    ) -> Option<bool> {
        let result =
            signer_cancel.run_until_cancelled(self.registry.is_registered_signer(signer)).await?;
        let registered = match result {
            Ok(registered) => registered,
            Err(error) => {
                warn!(
                    error = %error,
                    signer = %signer,
                    "failed to query registration state after transaction"
                );
                return Some(false);
            }
        };
        if registered {
            RegistrarMetrics::record_registration_stage(
                RegistrarMetrics::REGISTRATION_STAGE_TX_OBSERVED_REGISTERED,
            );
            RegistrarMetrics::registrations_total().increment(1);
        }
        Some(registered)
    }

    async fn sleep_before_retry(
        &self,
        retry: u32,
        signer: Address,
        operation: &'static str,
        error: &TxManagerError,
        signer_cancel: &CancellationToken,
    ) -> bool {
        let delay = self.retry_delay(retry);
        warn!(
            error = %error,
            signer = %signer,
            operation,
            retry,
            max_retries = self.max_tx_retries,
            delay = ?delay,
            "transaction submission failed, retrying"
        );
        signer_cancel.run_until_cancelled(tokio::time::sleep(delay)).await.is_some()
    }

    /// Queries onchain signers and deregisters orphans.
    pub async fn run_orphan_dereg(
        &self,
        protected_signers: &HashSet<Address>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let Some(registered_signers) =
            cancel.run_until_cancelled(self.registry.get_registered_signers()).await.transpose()?
        else {
            debug!("cancelled before loading registered signers for orphan deregistration");
            return Ok(());
        };
        for signer in
            registered_signers.into_iter().filter(|addr| !protected_signers.contains(addr))
        {
            if cancel.is_cancelled() {
                break;
            }
            let candidate = TxCandidate {
                tx_data: Bytes::from(
                    ITEEProverRegistry::deregisterSignerCall { signer }.abi_encode(),
                ),
                to: Some(self.registry_address),
                ..Default::default()
            };
            match self.tx_manager.send(candidate).await {
                Ok(receipt) if receipt.inner.status() => {
                    info!(signer = %signer, tx_hash = %receipt.transaction_hash, "signer deregistered");
                    RegistrarMetrics::deregistrations_total().increment(1);
                }
                Ok(receipt) => {
                    warn!(
                        signer = %signer,
                        tx_hash = %receipt.transaction_hash,
                        "deregistration transaction reverted"
                    );
                    RegistrarMetrics::processing_errors_total().increment(1);
                }
                Err(e) => {
                    warn!(error = %e, signer = %signer, "failed to deregister signer");
                    RegistrarMetrics::processing_errors_total().increment(1);
                }
            }
        }
        Ok(())
    }
}

impl<R, C, T> SignerManager<R, C, T>
where
    R: TEEProverRegistryClient + 'static,
    C: CertManagerClient + 'static,
    T: TxManager + 'static,
{
    /// Reconciles in-flight registration tasks against fetched prover signers.
    pub fn reconcile_proof_tasks(
        self: &Arc<Self>,
        resolution: &DiscoveryResolution,
        proof_tasks: &mut ProofTaskSet,
        cancel: &CancellationToken,
    ) {
        if cancel.is_cancelled() {
            return;
        }
        for (signer, task) in &mut proof_tasks.pending {
            if task.cancel.is_cancelled()
                || resolution.registerable.iter().any(|entry| entry.signer == *signer)
                || resolution.unresolved_instance_ids.contains(&task.instance_id)
            {
                continue;
            }
            info!(
                signer = %signer,
                instance = %task.instance_id,
                "cancelling registration task: signer no longer registerable"
            );
            task.cancel.cancel();
            RegistrarMetrics::proof_tasks_cancelled().increment(1);
        }
        for entry in &resolution.registerable {
            if proof_tasks.pending.contains_key(&entry.signer) {
                continue;
            }
            let signer_cancel = cancel.child_token();
            let manager = Arc::clone(self);
            let instance_id = entry.instance.instance_id.clone();
            let task_instance_id = instance_id.clone();
            let attestation = entry.attestation.clone();
            let task_cancel = signer_cancel.clone();
            let signer = entry.signer;
            let handle = proof_tasks.tasks.spawn(async move {
                let result = manager
                    .register_signer(&task_instance_id, signer, &attestation, &task_cancel)
                    .await;
                (signer, result)
            });
            proof_tasks.pending.insert(
                signer,
                PendingRegistration { instance_id, task_id: handle.id(), cancel: signer_cancel },
            );
            RegistrarMetrics::proof_tasks_spawned().increment(1);
        }
    }
}

#[cfg(test)]
mod tests {
    //! These hand-rolled mocks share one ordered state across contract reads and transaction
    //! writes, which `mockall` cannot express while transactions race through per-certificate locks.

    use std::{
        collections::VecDeque,
        sync::{
            OnceLock,
            atomic::{AtomicBool, Ordering},
        },
    };

    use alloy_primitives::Address;
    use async_trait::async_trait;
    use base_proof_contracts::{ContractError, ICertManager, VerifiedCert};
    use base_tx_manager::{SendHandle, TxManagerError};
    #[cfg(feature = "metrics")]
    use metrics_util::{
        MetricKind,
        debugging::{DebugValue, DebuggingRecorder},
    };

    use super::*;
    use crate::{
        DEFAULT_MAX_CONCURRENCY, RegisterableSigner,
        test_utils::{
            EP1, EP2, HARDHAT_KEY_0, HARDHAT_KEY_1, TEST_REGISTRY_ADDRESS, healthy_prover_instance,
            signer_from_private_key, stub_receipt_with_status,
        },
    };

    const TEST_INSTANCE: &str = "i-registrar-test";
    const TEST_CERT_MANAGER_ADDRESS: Address = Address::repeat_byte(0x22);
    const TEST_MAX_AGE: Duration = Duration::from_secs(3600);
    const TEST_RETRY_DELAY: Duration = Duration::from_secs(1);
    const SIGNER_A: Address = Address::repeat_byte(0xaa);
    const SIGNER_B: Address = Address::repeat_byte(0xbb);

    #[derive(Clone, Debug)]
    struct MockCertSpec {
        hash: B256,
        parent: B256,
        revocation_id: B256,
        kind: CertKind,
        verified: VerifiedCert,
    }

    #[derive(Clone, Debug)]
    enum MockTxOutcome {
        Success,
        Error(TxManagerError),
        ApplyThenError(TxManagerError),
        ApplyThenRevert,
    }

    #[derive(Debug, Default)]
    struct MockChainState {
        specs: HashMap<Vec<u8>, MockCertSpec>,
        cached: HashMap<B256, (B256, VerifiedCert)>,
        revoked: HashSet<B256>,
        registered: HashSet<Address>,
        sent: Vec<(Option<Address>, Bytes)>,
        outcomes: VecDeque<MockTxOutcome>,
        final_signer: Option<Address>,
        cert_reads: usize,
    }

    #[derive(Clone, Debug, Default)]
    struct MockChain(Arc<Mutex<MockChainState>>);

    impl MockChain {
        fn with_plan(plan: &RegistrationPlan) -> Self {
            let chain = Self::default();
            let root = MockCertSpec {
                hash: plan.root_cert_hash,
                parent: B256::ZERO,
                revocation_id: plan.root_cert_hash,
                kind: CertKind::Ca,
                verified: verified_cert(CertKind::Ca),
            };
            let mut state = chain.0.lock().unwrap();
            state.specs.insert(plan.root_cert.clone(), root.clone());
            state.cached.insert(root.hash, (root.parent, root.verified));
            for cert in &plan.certs {
                state.specs.insert(
                    cert.cert.clone(),
                    MockCertSpec {
                        hash: cert.cert_hash,
                        parent: cert.parent_cert_hash,
                        revocation_id: cert.revocation_id,
                        kind: cert.kind,
                        verified: verified_cert(cert.kind),
                    },
                );
            }
            drop(state);
            chain
        }

        fn precache(&self, plan: &RegistrationPlan, prefix_len: usize) {
            let mut state = self.0.lock().unwrap();
            for cert in plan.certs.iter().take(prefix_len) {
                let spec = state.specs.get(&cert.cert).unwrap().clone();
                state.cached.insert(spec.hash, (spec.parent, spec.verified));
            }
        }

        fn set_outcomes(&self, outcomes: impl IntoIterator<Item = MockTxOutcome>) {
            self.0.lock().unwrap().outcomes = outcomes.into_iter().collect();
        }

        fn tx_count_to(&self, address: Address) -> usize {
            self.0.lock().unwrap().sent.iter().filter(|(to, _)| *to == Some(address)).count()
        }

        fn sent(&self) -> Vec<(Option<Address>, Bytes)> {
            self.0.lock().unwrap().sent.clone()
        }

        fn cert_reads(&self) -> usize {
            self.0.lock().unwrap().cert_reads
        }

        fn apply(&self, candidate: &TxCandidate) {
            let mut state = self.0.lock().unwrap();
            let data = candidate.tx_data.as_ref();
            if data.starts_with(&ICertManager::verifyCACertWithHintsCall::SELECTOR) {
                let call = ICertManager::verifyCACertWithHintsCall::abi_decode(data).unwrap();
                let spec = state.specs.get(call.cert.as_ref()).unwrap().clone();
                assert_eq!(spec.kind, CertKind::Ca);
                assert_eq!(spec.parent, call.parentCertHash);
                state.cached.insert(spec.hash, (spec.parent, spec.verified));
            } else if data.starts_with(&ICertManager::verifyClientCertWithHintsCall::SELECTOR) {
                let call = ICertManager::verifyClientCertWithHintsCall::abi_decode(data).unwrap();
                let spec = state.specs.get(call.cert.as_ref()).unwrap().clone();
                assert_eq!(spec.kind, CertKind::Leaf);
                assert_eq!(spec.parent, call.parentCertHash);
                state.cached.insert(spec.hash, (spec.parent, spec.verified));
            } else if data.starts_with(&ICertManager::revokeCertCall::SELECTOR) {
                let call = ICertManager::revokeCertCall::abi_decode(data).unwrap();
                state.revoked.insert(call.certId);
            } else if data.starts_with(&ITEEProverRegistry::registerSignerCall::SELECTOR) {
                if let Some(signer) = state.final_signer {
                    state.registered.insert(signer);
                }
            } else if data.starts_with(&ITEEProverRegistry::deregisterSignerCall::SELECTOR) {
                let call = ITEEProverRegistry::deregisterSignerCall::abi_decode(data).unwrap();
                state.registered.remove(&call.signer);
            }
        }
    }

    #[derive(Clone, Debug)]
    struct MockRegistry {
        chain: MockChain,
        stall_get_registered: Arc<AtomicBool>,
    }

    #[async_trait]
    impl TEEProverRegistryClient for MockRegistry {
        fn address(&self) -> Address {
            TEST_REGISTRY_ADDRESS
        }

        async fn nitro_validator(&self) -> std::result::Result<Address, ContractError> {
            Ok(Address::repeat_byte(0x33))
        }

        async fn is_valid_signer(
            &self,
            signer: Address,
        ) -> std::result::Result<bool, ContractError> {
            self.is_registered_signer(signer).await
        }

        async fn is_registered_signer(
            &self,
            signer: Address,
        ) -> std::result::Result<bool, ContractError> {
            Ok(self.chain.0.lock().unwrap().registered.contains(&signer))
        }

        async fn get_registered_signers(&self) -> std::result::Result<Vec<Address>, ContractError> {
            if self.stall_get_registered.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            Ok(self.chain.0.lock().unwrap().registered.iter().copied().collect())
        }
    }

    #[derive(Clone, Debug)]
    struct MockCertManager {
        chain: MockChain,
    }

    impl MockCertManager {
        fn warm(
            &self,
            cert: &Bytes,
            parent_cert_hash: B256,
            kind: CertKind,
        ) -> std::result::Result<(B256, VerifiedCert), ContractError> {
            let mut state = self.chain.0.lock().unwrap();
            state.cert_reads += 1;
            let spec = state
                .specs
                .get(cert.as_ref())
                .ok_or_else(|| ContractError::validation("unknown certificate"))?
                .clone();
            let (cached_parent, verified) = state
                .cached
                .get(&spec.hash)
                .cloned()
                .ok_or_else(|| ContractError::validation("inverse hint underflow"))?;
            if cached_parent != parent_cert_hash {
                return Err(ContractError::validation("parent cert mismatch"));
            }
            if spec.kind != kind || verified.ca != (kind == CertKind::Ca) {
                return Err(ContractError::validation("cert is not a CA"));
            }
            if state.revoked.contains(&spec.revocation_id) {
                return Err(ContractError::validation("cert revoked"));
            }
            Ok((spec.hash, verified))
        }
    }

    #[async_trait]
    impl CertManagerClient for MockCertManager {
        fn address(&self) -> Address {
            TEST_CERT_MANAGER_ADDRESS
        }

        async fn verify_ca_cert_with_hints(
            &self,
            cert: Bytes,
            parent_cert_hash: B256,
            _signature_hints: Bytes,
        ) -> std::result::Result<B256, ContractError> {
            Ok(self.warm(&cert, parent_cert_hash, CertKind::Ca)?.0)
        }

        async fn verify_client_cert_with_hints(
            &self,
            cert: Bytes,
            parent_cert_hash: B256,
            _signature_hints: Bytes,
        ) -> std::result::Result<VerifiedCert, ContractError> {
            Ok(self.warm(&cert, parent_cert_hash, CertKind::Leaf)?.1)
        }

        async fn load_verified(
            &self,
            cert_hash: B256,
        ) -> std::result::Result<VerifiedCert, ContractError> {
            let mut state = self.chain.0.lock().unwrap();
            state.cert_reads += 1;
            Ok(state
                .cached
                .get(&cert_hash)
                .map(|(_, cert)| cert.clone())
                .unwrap_or_else(empty_verified_cert))
        }

        async fn is_revoked(&self, cert_id: B256) -> std::result::Result<bool, ContractError> {
            let mut state = self.chain.0.lock().unwrap();
            state.cert_reads += 1;
            Ok(state.revoked.contains(&cert_id))
        }

        async fn owner(&self) -> std::result::Result<Address, ContractError> {
            Ok(Address::ZERO)
        }

        async fn revoker(&self) -> std::result::Result<Address, ContractError> {
            Ok(Address::ZERO)
        }

        async fn compute_cert_id(&self, cert: Bytes) -> std::result::Result<B256, ContractError> {
            self.chain
                .0
                .lock()
                .unwrap()
                .specs
                .get(cert.as_ref())
                .map(|spec| spec.revocation_id)
                .ok_or_else(|| ContractError::validation("unknown certificate"))
        }
    }

    #[derive(Clone, Debug)]
    struct MockTxManager {
        chain: MockChain,
    }

    impl TxManager for MockTxManager {
        async fn send(&self, candidate: TxCandidate) -> base_tx_manager::SendResponse {
            let outcome = {
                let mut state = self.chain.0.lock().unwrap();
                state.sent.push((candidate.to, candidate.tx_data.clone()));
                state.outcomes.pop_front().unwrap_or(MockTxOutcome::Success)
            };
            match outcome {
                MockTxOutcome::Success => {
                    self.chain.apply(&candidate);
                    Ok(stub_receipt_with_status(true))
                }
                MockTxOutcome::Error(error) => Err(error),
                MockTxOutcome::ApplyThenError(error) => {
                    self.chain.apply(&candidate);
                    Err(error)
                }
                MockTxOutcome::ApplyThenRevert => {
                    self.chain.apply(&candidate);
                    Ok(stub_receipt_with_status(false))
                }
            }
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            unreachable!("registrar tests use synchronous transaction submission")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    type TestManager = SignerManager<MockRegistry, MockCertManager, MockTxManager>;

    fn verified_cert(kind: CertKind) -> VerifiedCert {
        VerifiedCert {
            ca: kind == CertKind::Ca,
            not_after: SignerManager::<(), (), ()>::now().0 + 10_000,
            max_path_len: if kind == CertKind::Ca { 3 } else { 0 },
            subject_hash: B256::repeat_byte(0x44),
            public_key: Bytes::from(vec![0x55; 96]),
        }
    }

    fn empty_verified_cert() -> VerifiedCert {
        VerifiedCert {
            ca: false,
            not_after: 0,
            max_path_len: 0,
            subject_hash: B256::ZERO,
            public_key: Bytes::new(),
        }
    }

    fn root_cert() -> Vec<u8> {
        static ROOT: OnceLock<Vec<u8>> = OnceLock::new();
        ROOT.get_or_init(|| {
            let attestation =
                hex::decode(include_str!("testdata/nitro_attestation.hex").trim()).unwrap();
            AttestationPlanner::prepare_registration_plan(&attestation).unwrap().root_cert
        })
        .clone()
    }

    fn synthetic_plan(signer: Address) -> RegistrationPlan {
        let (_, now_ms) = SignerManager::<(), (), ()>::now();
        let mut parent = PINNED_ROOT_CERT_HASH;
        let mut certs = Vec::new();
        for index in 1..=4u8 {
            let hash = B256::repeat_byte(index);
            certs.push(CertPlan {
                kind: if index == 4 { CertKind::Leaf } else { CertKind::Ca },
                label: format!("certificate {index}"),
                cert: vec![index],
                cert_hash: hash,
                parent_cert_hash: parent,
                revocation_id: B256::repeat_byte(index + 0x10),
            });
            parent = hash;
        }
        RegistrationPlan {
            signer,
            pcr0: vec![0x42; 48],
            timestamp: now_ms.saturating_sub(1_000),
            nonce: Some(TestManager::attestation_nonce_for(TEST_REGISTRY_ADDRESS, signer).to_vec()),
            root_cert_hash: PINNED_ROOT_CERT_HASH,
            root_cert: root_cert(),
            leaf_cert_hash: parent,
            attestation_tbs: vec![signer.as_slice()[19]],
            signature: vec![0x66; 96],
            certs,
        }
    }

    fn synthetic_hints() -> RegistrationHints {
        RegistrationHints {
            cert_signature_hints: (1..=4).map(|index| vec![index; 48]).collect(),
            attestation_hints: vec![0x77; 48],
        }
    }

    fn manager_with_plan(plan: &RegistrationPlan) -> (Arc<TestManager>, MockChain) {
        manager_with_config(plan, DEFAULT_MAX_TX_RETRIES, TEST_RETRY_DELAY, TEST_MAX_AGE)
    }

    fn manager_with_config(
        plan: &RegistrationPlan,
        max_tx_retries: u32,
        tx_retry_delay: Duration,
        max_attestation_age: Duration,
    ) -> (Arc<TestManager>, MockChain) {
        let chain = MockChain::with_plan(plan);
        let manager = SignerManager::new(
            MockRegistry {
                chain: chain.clone(),
                stall_get_registered: Arc::new(AtomicBool::new(false)),
            },
            MockCertManager { chain: chain.clone() },
            MockTxManager { chain: chain.clone() },
            SignerManagerConfig {
                registry_address: TEST_REGISTRY_ADDRESS,
                max_concurrency: DEFAULT_MAX_CONCURRENCY,
                max_tx_retries,
                tx_retry_delay,
                max_attestation_age,
                crl_checks_enabled: false,
            },
        )
        .unwrap();
        (Arc::new(manager), chain)
    }

    async fn register_prepared(manager: &TestManager, plan: RegistrationPlan) -> Result<()> {
        manager
            .register_plan(
                TEST_INSTANCE,
                plan.signer,
                plan,
                Some(synthetic_hints()),
                &CancellationToken::new(),
            )
            .await
    }

    #[tokio::test]
    async fn cold_chain_submits_four_cache_transactions_then_registration() {
        let plan = synthetic_plan(SIGNER_A);
        let (manager, chain) = manager_with_plan(&plan);

        register_prepared(&manager, plan).await.unwrap();

        assert_eq!(chain.tx_count_to(TEST_CERT_MANAGER_ADDRESS), 4);
        assert_eq!(chain.tx_count_to(TEST_REGISTRY_ADDRESS), 1);
    }

    #[tokio::test]
    async fn every_cache_prefix_submits_only_missing_suffix_and_registration() {
        for prefix in 0..=4 {
            let plan = synthetic_plan(SIGNER_A);
            let (manager, chain) = manager_with_plan(&plan);
            chain.precache(&plan, prefix);

            register_prepared(&manager, plan).await.unwrap();

            assert_eq!(chain.tx_count_to(TEST_CERT_MANAGER_ADDRESS), 4 - prefix, "prefix {prefix}");
            assert_eq!(chain.tx_count_to(TEST_REGISTRY_ADDRESS), 1, "prefix {prefix}");
        }
    }

    #[tokio::test]
    async fn cached_cas_and_new_leaf_submit_two_transactions() {
        let plan = synthetic_plan(SIGNER_A);
        let (manager, chain) = manager_with_plan(&plan);
        chain.precache(&plan, 3);

        register_prepared(&manager, plan).await.unwrap();

        assert_eq!(chain.sent().len(), 2);
        assert_eq!(chain.tx_count_to(TEST_CERT_MANAGER_ADDRESS), 1);
        assert_eq!(chain.tx_count_to(TEST_REGISTRY_ADDRESS), 1);
    }

    #[tokio::test]
    async fn fully_warm_chain_submits_only_registration() {
        let plan = synthetic_plan(SIGNER_A);
        let (manager, chain) = manager_with_plan(&plan);
        chain.precache(&plan, 4);

        register_prepared(&manager, plan).await.unwrap();

        assert_eq!(chain.sent().len(), 1);
        assert_eq!(chain.tx_count_to(TEST_REGISTRY_ADDRESS), 1);
    }

    #[tokio::test]
    async fn concurrent_registrations_cache_shared_chain_once() {
        let plan_a = synthetic_plan(SIGNER_A);
        let plan_b = synthetic_plan(SIGNER_B);
        let (manager, chain) = manager_with_plan(&plan_a);
        {
            let mut state = chain.0.lock().unwrap();
            for cert in &plan_b.certs {
                state.specs.entry(cert.cert.clone()).or_insert_with(|| MockCertSpec {
                    hash: cert.cert_hash,
                    parent: cert.parent_cert_hash,
                    revocation_id: cert.revocation_id,
                    kind: cert.kind,
                    verified: verified_cert(cert.kind),
                });
            }
        }
        let first = {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move { register_prepared(&manager, plan_a).await })
        };
        let second = {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move { register_prepared(&manager, plan_b).await })
        };

        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();

        assert_eq!(chain.tx_count_to(TEST_CERT_MANAGER_ADDRESS), 4);
        assert_eq!(chain.tx_count_to(TEST_REGISTRY_ADDRESS), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguous_cache_error_and_reverted_receipt_reread_usable_state() {
        for first_outcome in [
            MockTxOutcome::ApplyThenError(TxManagerError::Rpc("receipt timeout".into())),
            MockTxOutcome::ApplyThenRevert,
        ] {
            let plan = synthetic_plan(SIGNER_A);
            let (manager, chain) = manager_with_plan(&plan);
            chain.set_outcomes([first_outcome]);

            register_prepared(&manager, plan).await.unwrap();

            assert_eq!(chain.tx_count_to(TEST_CERT_MANAGER_ADDRESS), 4);
            assert_eq!(chain.tx_count_to(TEST_REGISTRY_ADDRESS), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguous_final_error_and_reverted_receipt_observe_registration() {
        for final_outcome in [
            MockTxOutcome::ApplyThenError(TxManagerError::Rpc("receipt timeout".into())),
            MockTxOutcome::ApplyThenRevert,
        ] {
            let plan = synthetic_plan(SIGNER_A);
            let (manager, chain) = manager_with_plan(&plan);
            chain.0.lock().unwrap().final_signer = Some(SIGNER_A);
            chain.set_outcomes(
                std::iter::repeat_n(MockTxOutcome::Success, 4).chain([final_outcome]),
            );

            register_prepared(&manager, plan).await.unwrap();

            assert_eq!(chain.tx_count_to(TEST_REGISTRY_ADDRESS), 1);
        }
    }

    #[tokio::test]
    async fn expired_cached_certificate_is_rejected_without_transaction() {
        let plan = synthetic_plan(SIGNER_A);
        let (manager, chain) = manager_with_plan(&plan);
        chain.precache(&plan, 1);
        chain.0.lock().unwrap().cached.get_mut(&plan.certs[0].cert_hash).unwrap().1.not_after = 1;

        let result = register_prepared(&manager, plan).await;

        assert!(matches!(result, Err(RegistrarError::ExpiredCertificate { .. })));
        assert!(chain.sent().is_empty());
    }

    #[tokio::test]
    async fn root_and_certificate_revocations_are_rejected_before_cache_transactions() {
        for cert_id in [PINNED_ROOT_CERT_HASH, B256::repeat_byte(0x12)] {
            let plan = synthetic_plan(SIGNER_A);
            let (manager, chain) = manager_with_plan(&plan);
            chain.0.lock().unwrap().revoked.insert(cert_id);

            let result = register_prepared(&manager, plan).await;

            assert!(matches!(result, Err(RegistrarError::RevokedCertificate { .. })));
            assert!(chain.sent().is_empty());
        }
    }

    #[tokio::test]
    async fn already_registered_signer_still_checks_revocation_state() {
        let plan = synthetic_plan(SIGNER_A);
        let (manager, chain) = manager_with_plan(&plan);
        {
            let mut state = chain.0.lock().unwrap();
            state.registered.insert(SIGNER_A);
            state.revoked.insert(plan.certs[0].revocation_id);
        }

        let result = register_prepared(&manager, plan).await;

        assert!(matches!(result, Err(RegistrarError::RevokedCertificate { .. })));
        assert!(chain.sent().is_empty());
    }

    #[tokio::test]
    async fn cached_parent_mismatch_is_rejected_without_recache_transaction() {
        let plan = synthetic_plan(SIGNER_A);
        let (manager, chain) = manager_with_plan(&plan);
        chain.precache(&plan, 1);
        chain.0.lock().unwrap().cached.get_mut(&plan.certs[0].cert_hash).unwrap().0 =
            B256::repeat_byte(0xfe);

        let result = register_prepared(&manager, plan).await;

        assert!(
            matches!(result, Err(RegistrarError::InvalidAttestationProof(reason)) if reason.contains("parent cert mismatch"))
        );
        assert!(chain.sent().is_empty());
    }

    #[tokio::test]
    async fn real_attestation_signer_and_nonce_rejections_happen_before_contract_reads() {
        let attestation =
            hex::decode(include_str!("testdata/nitro_attestation.hex").trim()).unwrap();
        let parsed = AttestationPlanner::prepare_registration_plan(&attestation).unwrap();
        for signer in [SIGNER_B, parsed.signer] {
            let plan = synthetic_plan(signer);
            let (manager, chain) = manager_with_plan(&plan);

            let result = manager
                .register_signer(TEST_INSTANCE, signer, &attestation, &CancellationToken::new())
                .await;

            assert!(matches!(result, Err(RegistrarError::InvalidAttestationProof(_))));
            assert_eq!(chain.cert_reads(), 0);
            assert!(chain.sent().is_empty());
        }
    }

    #[tokio::test]
    async fn pcr_future_and_stale_plans_are_rejected_before_transactions() {
        let (_, now_ms) = TestManager::now();
        for mutation in 0..3 {
            let mut plan = synthetic_plan(SIGNER_A);
            match mutation {
                0 => plan.pcr0 = vec![0; 47],
                1 => plan.timestamp = now_ms + 10_000,
                _ => {
                    plan.timestamp = now_ms.saturating_sub(TEST_MAX_AGE.as_millis() as u64 + 2_000)
                }
            }
            let (manager, chain) = manager_with_plan(&plan);

            let result = register_prepared(&manager, plan).await;

            assert!(result.is_err());
            assert!(chain.sent().is_empty());
        }
    }

    #[test]
    fn same_second_attestation_is_not_rejected_locally() {
        let mut plan = synthetic_plan(SIGNER_A);
        let (manager, _) = manager_with_plan(&plan);
        plan.timestamp = TestManager::now().1;

        manager.validate_plan(SIGNER_A, &plan).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn retryable_cache_error_retries_same_calldata_with_backoff() {
        let plan = synthetic_plan(SIGNER_A);
        let (manager, chain) = manager_with_config(&plan, 2, TEST_RETRY_DELAY, TEST_MAX_AGE);
        chain.set_outcomes([MockTxOutcome::Error(TxManagerError::Rpc("temporary".into()))]);
        let start = tokio::time::Instant::now();

        register_prepared(&manager, plan).await.unwrap();

        assert_eq!(start.elapsed(), TEST_RETRY_DELAY);
        assert_eq!(chain.tx_count_to(TEST_CERT_MANAGER_ADDRESS), 5);
        let sent = chain.sent();
        assert_eq!(sent[0], sent[1]);
    }

    /// Every cache transaction counted as `submitted` must also report exactly one terminal
    /// outcome, otherwise `cert_cache_tx_total` cannot be reconciled on a dashboard.
    #[cfg(feature = "metrics")]
    #[test]
    fn cache_tx_submitted_count_matches_terminal_outcomes() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap();
            runtime.block_on(async {
                for outcome in [
                    MockTxOutcome::Success,
                    MockTxOutcome::Error(TxManagerError::Rpc("temporary".into())),
                    MockTxOutcome::ApplyThenError(TxManagerError::Rpc("receipt timeout".into())),
                    MockTxOutcome::ApplyThenRevert,
                ] {
                    let plan = synthetic_plan(SIGNER_A);
                    let (manager, chain) = manager_with_plan(&plan);
                    chain.set_outcomes([outcome]);
                    register_prepared(&manager, plan).await.unwrap();
                }
            });
        });

        let snapshot = snapshotter.snapshot().into_vec();
        let count = |outcome: &str| -> u64 {
            snapshot
                .iter()
                .filter(|(key, _, _, _)| {
                    key.kind() == MetricKind::Counter
                        && key.key().name() == "base_registrar.cert_cache_tx_total"
                        && key
                            .key()
                            .labels()
                            .any(|label| label.key() == "outcome" && label.value() == outcome)
                })
                .map(|(_, _, _, value)| match value {
                    DebugValue::Counter(count) => *count,
                    other => panic!("expected a counter, got {other:?}"),
                })
                .sum()
        };

        let submitted = count(RegistrarMetrics::TX_OUTCOME_SUBMITTED);
        let terminal: u64 = [
            RegistrarMetrics::TX_OUTCOME_SUCCEEDED,
            RegistrarMetrics::TX_OUTCOME_OBSERVED_CACHED,
            RegistrarMetrics::TX_OUTCOME_REVERTED,
            RegistrarMetrics::TX_OUTCOME_RETRY,
            RegistrarMetrics::TX_OUTCOME_FAILED,
            RegistrarMetrics::TX_OUTCOME_CANCELLED,
        ]
        .into_iter()
        .map(count)
        .sum();

        assert_eq!(submitted, terminal, "submitted cache transactions must all reach a terminal");
        assert!(count(RegistrarMetrics::TX_OUTCOME_RETRY) > 0, "retry exit was not exercised");
        assert!(
            count(RegistrarMetrics::TX_OUTCOME_OBSERVED_CACHED) > 0,
            "ambiguous-receipt exit was not exercised"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_during_retry_backoff_stops_without_another_send() {
        let plan = synthetic_plan(SIGNER_A);
        let (manager, chain) = manager_with_plan(&plan);
        chain.set_outcomes([MockTxOutcome::Error(TxManagerError::Rpc("temporary".into()))]);
        let cancel = CancellationToken::new();
        let cancel_task = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            cancel_task.cancel();
        });

        manager
            .register_plan(TEST_INSTANCE, SIGNER_A, plan, Some(synthetic_hints()), &cancel)
            .await
            .unwrap();

        assert_eq!(chain.sent().len(), 1);
    }

    fn resolution(entries: &[(&str, Address)]) -> DiscoveryResolution {
        DiscoveryResolution {
            registerable: entries
                .iter()
                .map(|(endpoint, signer)| RegisterableSigner {
                    instance: healthy_prover_instance(endpoint),
                    signer: *signer,
                    attestation: b"synthetic-attestation".to_vec(),
                })
                .collect(),
            active_signers: HashSet::new(),
            unresolved_instance_ids: HashSet::new(),
        }
    }

    fn spawn_pending(
        tasks: &mut ProofTaskSet,
        signer: Address,
        instance_id: &str,
    ) -> CancellationToken {
        let cancel = CancellationToken::new();
        let handle = tasks.tasks.spawn(std::future::pending::<(Address, Result<()>)>());
        tasks.pending.insert(
            signer,
            PendingRegistration {
                instance_id: instance_id.into(),
                task_id: handle.id(),
                cancel: cancel.clone(),
            },
        );
        cancel
    }

    async fn abort_tasks(tasks: &mut ProofTaskSet) {
        tasks.tasks.abort_all();
        while tasks.tasks.join_next().await.is_some() {}
        tasks.pending.clear();
    }

    #[tokio::test]
    async fn reconciliation_deduplicates_preserves_unresolved_and_cancels_absent_tasks() {
        let plan = synthetic_plan(SIGNER_A);
        let (manager, _) = manager_with_plan(&plan);
        let mut tasks = ProofTaskSet::default();
        let pending_cancel = spawn_pending(&mut tasks, SIGNER_A, TEST_INSTANCE);
        let unresolved = DiscoveryResolution {
            unresolved_instance_ids: HashSet::from([TEST_INSTANCE.into()]),
            ..Default::default()
        };
        manager.reconcile_proof_tasks(&unresolved, &mut tasks, &CancellationToken::new());
        assert!(!pending_cancel.is_cancelled());

        manager.reconcile_proof_tasks(
            &resolution(&[(EP1, SIGNER_A), (EP2, SIGNER_A)]),
            &mut tasks,
            &CancellationToken::new(),
        );
        assert_eq!(tasks.pending.len(), 1);

        manager.reconcile_proof_tasks(
            &DiscoveryResolution::default(),
            &mut tasks,
            &CancellationToken::new(),
        );
        assert!(pending_cancel.is_cancelled());
        abort_tasks(&mut tasks).await;
    }

    #[tokio::test]
    async fn reconciliation_spawns_one_task_per_distinct_signer() {
        let plan = synthetic_plan(SIGNER_A);
        let (manager, _) = manager_with_plan(&plan);
        let mut tasks = ProofTaskSet::default();

        manager.reconcile_proof_tasks(
            &resolution(&[(EP1, SIGNER_A), (EP2, SIGNER_B)]),
            &mut tasks,
            &CancellationToken::new(),
        );

        assert_eq!(tasks.pending.len(), 2);
        abort_tasks(&mut tasks).await;
    }

    #[tokio::test]
    async fn orphan_deregistration_submits_only_unprotected_signers() {
        let plan = synthetic_plan(SIGNER_A);
        let (manager, chain) = manager_with_plan(&plan);
        chain.0.lock().unwrap().registered.extend([SIGNER_A, SIGNER_B]);

        manager
            .run_orphan_dereg(&HashSet::from([SIGNER_A]), &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(chain.tx_count_to(TEST_REGISTRY_ADDRESS), 1);
        let sent = chain.sent();
        let call = ITEEProverRegistry::deregisterSignerCall::abi_decode(&sent[0].1).unwrap();
        assert_eq!(call.signer, SIGNER_B);
    }

    #[tokio::test]
    async fn orphan_deregistration_stops_while_registry_read_is_cancelled() {
        let plan = synthetic_plan(SIGNER_A);
        let (manager, chain) = manager_with_plan(&plan);
        manager.registry.stall_get_registered.store(true, Ordering::SeqCst);
        let cancel = CancellationToken::new();
        let protected = HashSet::new();
        let run = manager.run_orphan_dereg(&protected, &cancel);
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("orphan pass returned before cancellation: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        cancel.cancel();

        run.await.unwrap();
        assert!(chain.sent().is_empty());
    }

    #[test]
    fn nonce_is_bound_to_registry_and_signer() {
        let signer = signer_from_private_key(&HARDHAT_KEY_0);
        assert_ne!(
            TestManager::attestation_nonce_for(TEST_REGISTRY_ADDRESS, signer),
            TestManager::attestation_nonce_for(Address::repeat_byte(2), signer)
        );
        assert_ne!(
            TestManager::attestation_nonce_for(TEST_REGISTRY_ADDRESS, signer),
            TestManager::attestation_nonce_for(
                TEST_REGISTRY_ADDRESS,
                signer_from_private_key(&HARDHAT_KEY_1)
            )
        );
    }
}
