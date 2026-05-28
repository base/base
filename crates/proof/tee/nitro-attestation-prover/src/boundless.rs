//! [`BoundlessProver`] — proving backend using the Boundless marketplace.
//!
//! Submits proof requests to the Boundless decentralised proving marketplace
//! and polls for fulfillment with a configurable timeout.
//!
//! # Proof recovery
//!
//! When the registrar's instance rotates mid-proof (e.g. during an ASG
//! deployment), in-flight Boundless proofs would normally be lost — the
//! new instance has no memory of the old request and submits (and pays
//! for) a brand-new one. The recovery mechanism avoids this by deriving
//! request IDs deterministically from the target signer address, so
//! that the new instance can rediscover and resume any in-flight proof
//! without external state.
//!
//! See [`BoundlessProver::derive_request_index`] and the
//! [`generate_proof_for_signer`](AttestationProofProvider::generate_proof_for_signer)
//! override on [`BoundlessProver`] for details.

use std::{collections::HashSet, fmt, sync::Arc, time::Duration};

use alloy_primitives::{Address, B256, Bytes, keccak256};
use base_proof_tee_nitro_verifier::{VerifierInput, VerifierJournal};
// `boundless-market` re-exports `alloy` (`pub use alloy`) but does not
// re-export `DynProvider` directly — access it via the SDK's alloy so
// the type in our alias matches the one inside `Client`.
use boundless_market::alloy::providers::DynProvider;
use boundless_market::{
    Client, NotProvided,
    alloy::signers::local::PrivateKeySigner,
    contracts::{Predicate, RequestId, RequestStatus},
    price_oracle::Amount,
    request_builder::{OfferParams, RequestParams, RequirementParams, StandardRequestBuilder},
};
use risc0_zkvm::sha::Digest;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use url::Url;

use crate::{AttestationProof, AttestationProofProvider, ProverError, Result};

/// Concrete [`Client`] type produced by the builder chain used in
/// [`BoundlessProver`]. The uploader is [`NotProvided`] because we
/// use inline inputs (stdin) rather than uploading to external storage.
///
/// NOTE: `DynProvider` and `StandardDownloader` must come from the
/// same crate versions that `boundless-market` uses internally.
/// `DynProvider` is accessed via the SDK's `alloy` re-export;
/// `StandardDownloader` is directly re-exported by `boundless-market`.
type BoundlessClient = Client<
    DynProvider,
    NotProvided,
    boundless_market::StandardDownloader,
    StandardRequestBuilder<DynProvider, NotProvided, boundless_market::StandardDownloader>,
    PrivateKeySigner,
>;

/// Attestation prover using the Boundless marketplace.
///
/// Submits proof requests with a guest program URL (IPFS or HTTP) and
/// polls for fulfillment within a configurable timeout.
#[derive(Clone)]
pub struct BoundlessProver {
    /// Ethereum RPC URL for the Boundless settlement chain.
    pub rpc_url: Url,
    /// Signer for Boundless Network proving fees.
    pub signer: PrivateKeySigner,
    /// HTTP(S) URL where the guest ELF is hosted (e.g. a Pinata or Boundless IPFS gateway URL).
    pub verifier_program_url: Url,
    /// Expected image ID of the guest program.
    pub image_id: [u32; 8],
    /// Interval between fulfillment status checks.
    pub poll_interval: Duration,
    /// Maximum time to wait for proof fulfillment.
    pub timeout: Duration,
    /// Number of trusted certificates in the chain (typically 1 for root-only).
    pub trusted_certs_prefix_len: u8,
    /// Maximum number of deterministic request-ID slots to probe when
    /// recovering in-flight proofs after an instance rotation.
    pub max_recovery_attempts: u32,
    /// Maximum age of an attestation timestamp for a recovered proof to
    /// be considered fresh enough for on-chain submission. Proofs whose
    /// journal timestamp is older than this are skipped during recovery.
    /// Should be set slightly below the on-chain `MAX_AGE` to account
    /// for clock skew and processing time.
    pub max_attestation_age: Duration,
    /// Optional minimum Boundless offer price for each submitted proof request.
    pub offer_min_price: Option<Amount>,
    /// Optional maximum Boundless offer price for each submitted proof request.
    pub offer_max_price: Option<Amount>,
    /// Optional duration in seconds for the Boundless offer price to
    /// ramp from `offer_min_price` to `offer_max_price`. When unset
    /// the Boundless SDK derives a cycle-count-based ramp; set to `0`
    /// to eliminate the ramp entirely (max price offered immediately).
    pub offer_ramp_up_period_secs: Option<u32>,
    /// Optional maximum time, in seconds, that a prover that locks a
    /// request has to deliver the proof before forfeiting its stake bond
    /// and the request opening up to permissionless secondary
    /// fulfillment. Also the deadline for any prover to lock the request
    /// in the first place — locking and delivery share a single
    /// deadline at `rampUpStart + lockTimeout`. When unset, the
    /// Boundless SDK derives a recommended value from the program's
    /// cycle count.
    pub offer_lock_timeout_secs: Option<u32>,
    /// Delay, in seconds, between request submission and the moment
    /// bidding is allowed to begin (`Offer.rampUpStart`).
    ///
    /// Defaults to `0` so that bidding opens immediately at submission,
    /// eliminating the SDK's default "pre-bid flat period" (visible on
    /// the Boundless explorer as the gap before the price-ramp begins).
    /// With `0`, the fastest prover can lock as soon as it finishes
    /// executing — at the cost of fewer provers seeing the request
    /// before lock. The value is added to the current wall-clock time
    /// per request to produce `Offer.rampUpStart`.
    pub offer_bidding_start_delay_secs: u64,
    /// Serialises the `submit_onchain` call so that concurrent proof
    /// requests do not race on the Boundless wallet nonce. The lock is
    /// released immediately after submission, allowing the long-running
    /// fulfillment poll to proceed concurrently.
    pub submit_lock: Arc<Mutex<()>>,
    /// Signers whose recovered proofs have been rejected on-chain.
    /// When a signer is in this set, recovery is skipped and a fresh
    /// proof is generated instead. Cleared on process restart, giving
    /// recovered proofs one new attempt after each restart.
    pub recovery_blocked: Arc<std::sync::Mutex<HashSet<Address>>>,
}

impl fmt::Debug for BoundlessProver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundlessProver")
            .field("rpc_url", &self.rpc_url.origin().unicode_serialization())
            .field("signer", &self.signer.address())
            .field(
                "verifier_program_url",
                &self.verifier_program_url.origin().unicode_serialization(),
            )
            .field("image_id", &self.image_id)
            .field("poll_interval", &self.poll_interval)
            .field("timeout", &self.timeout)
            .field("trusted_certs_prefix_len", &self.trusted_certs_prefix_len)
            .field("max_recovery_attempts", &self.max_recovery_attempts)
            .field("max_attestation_age", &self.max_attestation_age)
            .field("offer_min_price", &self.offer_min_price)
            .field("offer_max_price", &self.offer_max_price)
            .field("offer_ramp_up_period_secs", &self.offer_ramp_up_period_secs)
            .field("offer_lock_timeout_secs", &self.offer_lock_timeout_secs)
            .field("offer_bidding_start_delay_secs", &self.offer_bidding_start_delay_secs)
            .finish()
    }
}

impl BoundlessProver {
    /// Derives a deterministic `u32` index for a Boundless request ID
    /// from the target signer address and an attempt counter.
    ///
    /// The index is the first 4 bytes of `keccak256(signer_address ||
    /// attempt)` interpreted as big-endian `u32`. This gives each
    /// (signer, attempt) pair a collision-resistant slot in the Boundless
    /// request-ID space without requiring any persisted state.
    ///
    /// Note: the index is compressed to 32 bits, so collisions are
    /// theoretically possible but astronomically unlikely for the small
    /// number of slots probed per signer (governed by
    /// [`max_recovery_attempts`](Self::max_recovery_attempts)).
    pub fn derive_request_index(signer_address: Address, attempt: u32) -> u32 {
        let mut buf = [0u8; 24]; // 20 bytes address + 4 bytes attempt
        buf[..20].copy_from_slice(signer_address.as_slice());
        buf[20..].copy_from_slice(&attempt.to_be_bytes());
        let hash = keccak256(buf);
        u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]])
    }

    /// Checks whether an error from the Boundless SDK is the
    /// `RequestIsNotLocked` revert caused by the TOCTOU race in
    /// `get_status()`.
    ///
    /// Because the SDK wraps the contract revert in opaque error types
    /// we must resort to string matching. Both `Display` and `Debug`
    /// representations are searched case-insensitively to be resilient
    /// against upstream formatting changes.
    fn is_request_not_locked_error(e: &dyn std::error::Error) -> bool {
        const NEEDLE: &str = "requestisnotlocked";
        let display = format!("{e}");
        if display.to_ascii_lowercase().contains(NEEDLE) {
            return true;
        }
        let debug = format!("{e:?}");
        debug.to_ascii_lowercase().contains(NEEDLE)
    }

    /// Returns the current Unix timestamp in seconds, used to compute
    /// `Offer.rampUpStart` as `now + offer_bidding_start_delay_secs`
    /// immediately before each on-chain submission. Factored out so
    /// tests can bracket calls between `before`/`after` snapshots to
    /// assert the resulting timestamp falls in the expected window.
    fn now_unix_secs() -> u64 {
        // Matches the fail-soft pattern used elsewhere in this file
        // (`effective_expiry`, `is_journal_fresh`): a pre-epoch system
        // clock is unrealistic, and on a long-running registrar
        // returning 0 here is preferable to panicking — the resulting
        // bidding_start would be rejected downstream rather than
        // crashing the process.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// Applies the time-independent Boundless offer overrides to
    /// request params.
    ///
    /// `min_price`, `max_price`, `ramp_up_period`, and `lock_timeout`
    /// are only passed through when explicitly configured; when unset,
    /// the Boundless SDK derives sensible values from the workload's
    /// cycle count.
    ///
    /// `bidding_start` is intentionally **not** set here: it is a
    /// wall-clock-relative deadline anchor (the SDK computes
    /// `Offer.lockTimeout = bidding_start + lock_timeout_secs` and
    /// `Offer.timeout = bidding_start + timeout_secs`), so capturing
    /// it at params-build time would let the recovery scan,
    /// `submit_lock` contention, and preflight execution consume the
    /// lock/expiry window before the request is even submitted. It is
    /// instead set by [`apply_bidding_start`](Self::apply_bidding_start)
    /// immediately before `client.submit_onchain` inside the
    /// `submit_lock` guard.
    fn apply_offer_config(&self, params: RequestParams) -> RequestParams {
        let mut offer = OfferParams::builder();
        if let Some(min_price) = &self.offer_min_price {
            offer.min_price(min_price.clone());
        }
        if let Some(max_price) = &self.offer_max_price {
            offer.max_price(max_price.clone());
        }
        if let Some(ramp_up_period) = self.offer_ramp_up_period_secs {
            offer.ramp_up_period(ramp_up_period);
        }
        if let Some(lock_timeout) = self.offer_lock_timeout_secs {
            offer.lock_timeout(lock_timeout);
        }

        params.with_offer(offer)
    }

    /// Sets `Offer.rampUpStart` to `now_unix_secs() + offer_bidding_start_delay_secs`
    /// on the given params, mutating the existing offer in place rather
    /// than replacing it (so prior `apply_offer_config` settings are
    /// preserved).
    ///
    /// Must be called as late as possible — ideally inside the
    /// `submit_lock` guard, immediately before `client.submit_onchain`
    /// — so that the deadline anchor is fresh at the moment the
    /// request actually hits the chain. Calling it earlier (e.g. when
    /// the params are first built) lets recovery RPC latency and lock
    /// contention silently consume the lock/expiry window before the
    /// request exists, which can result in requests submitted with
    /// little or no remaining lock window.
    fn apply_bidding_start(&self, mut params: RequestParams) -> RequestParams {
        params.offer.bidding_start =
            Some(Self::now_unix_secs().saturating_add(self.offer_bidding_start_delay_secs));
        params
    }

    /// Fetches and ABI-encodes the set inclusion receipt for a fulfilled
    /// Boundless request. Shared between the recovery and fresh-submission
    /// paths.
    async fn fetch_and_encode_receipt(
        &self,
        client: &BoundlessClient,
        request_id: alloy_primitives::U256,
    ) -> Result<AttestationProof> {
        let image_id_bytes: [u8; 32] = Digest::from(self.image_id).into();
        let image_id_b256 = B256::from(image_id_bytes);

        // Retry only the RPC fetch — this is the transient-failure path.
        // abi_encode_seal below is deterministic and must not be retried.
        const MAX_RECEIPT_FETCH_RETRIES: u32 = 60;
        const RECEIPT_FETCH_RETRY_DELAY: Duration = Duration::from_secs(5);

        let mut receipt_retries = 0;
        let (journal, receipt) = loop {
            match client.fetch_set_inclusion_receipt(request_id, image_id_b256, None, None).await {
                Ok(result) => break result,
                Err(e) if receipt_retries < MAX_RECEIPT_FETCH_RETRIES => {
                    receipt_retries += 1;
                    warn!(
                        error = %e,
                        error_debug = ?e,
                        request_id = %request_id,
                        image_id = ?self.image_id,
                        retry = receipt_retries,
                        max_retries = MAX_RECEIPT_FETCH_RETRIES,
                        delay = ?RECEIPT_FETCH_RETRY_DELAY,
                        "transient receipt fetch failure, retrying"
                    );
                    tokio::time::sleep(RECEIPT_FETCH_RETRY_DELAY).await;
                }
                Err(e) => {
                    return Err(ProverError::Boundless(format!(
                        "failed to fetch set inclusion receipt: {e}"
                    )));
                }
            }
        };

        let encoded_seal = receipt.abi_encode_seal().map_err(|e| {
            warn!(
                error = %e,
                error_debug = ?e,
                request_id = %request_id,
                "failed to ABI-encode set inclusion seal"
            );
            ProverError::Boundless(format!("failed to encode set inclusion seal: {e}"))
        })?;

        let proof_bytes = Bytes::from(encoded_seal);

        info!(
            request_id = %request_id,
            journal_len = journal.len(),
            seal_len = proof_bytes.len(),
            "set inclusion receipt fetched and seal encoded successfully"
        );

        Ok(AttestationProof { output: journal, proof_bytes })
    }

    /// Waits for fulfillment of a locked request with the TOCTOU retry
    /// logic, then fetches the set inclusion receipt. Shared between the
    /// recovery and fresh-submission paths.
    async fn wait_and_fetch(
        &self,
        client: &BoundlessClient,
        request_id: alloy_primitives::U256,
        effective_expiry: u64,
    ) -> Result<AttestationProof> {
        const MAX_RACE_RETRIES: u32 = 3;
        let mut race_retries = 0;
        let _fulfillment = loop {
            match client
                .wait_for_request_fulfillment(request_id, self.poll_interval, effective_expiry)
                .await
            {
                Ok(f) => break f,
                Err(e) => {
                    if Self::is_request_not_locked_error(&e) && race_retries < MAX_RACE_RETRIES {
                        race_retries += 1;
                        warn!(
                            error = %e,
                            request_id = %request_id,
                            retry = race_retries,
                            max_retries = MAX_RACE_RETRIES,
                            "RequestIsNotLocked race condition, retrying fulfillment poll"
                        );
                        continue;
                    }
                    warn!(
                        error = %e,
                        error_debug = ?e,
                        request_id = %request_id,
                        effective_expiry,
                        timeout = ?self.timeout,
                        poll_interval = ?self.poll_interval,
                        "proof fulfillment failed"
                    );
                    return Err(ProverError::Boundless(format!("fulfillment failed: {e}")));
                }
            }
        };

        info!(request_id = %request_id, "fulfillment confirmed, fetching set inclusion receipt");

        self.fetch_and_encode_receipt(client, request_id).await
    }

    /// Builds the Boundless [`Client`] and [`RequestParams`] from the
    /// attestation bytes. Shared between `generate_proof` and
    /// `generate_proof_for_signer` to avoid duplicating the setup logic.
    async fn build_client_and_params(
        &self,
        attestation_bytes: &[u8],
    ) -> Result<(BoundlessClient, RequestParams)> {
        let input = VerifierInput {
            trustedCertsPrefixLen: self.trusted_certs_prefix_len,
            attestationReport: Bytes::copy_from_slice(attestation_bytes),
        };
        let input_bytes = input.encode();
        let image_id = Digest::from(self.image_id);

        info!(
            image_id = ?self.image_id,
            input_len = input_bytes.len(),
            attestation_len = attestation_bytes.len(),
            rpc_url = %self.rpc_url.origin().unicode_serialization(),
            boundless_wallet = %self.signer.address(),
            program_url = %self.verifier_program_url.origin().unicode_serialization(),
            timeout = ?self.timeout,
            poll_interval = ?self.poll_interval,
            trusted_certs_prefix_len = self.trusted_certs_prefix_len,
            "building Boundless client and request params"
        );

        let client = Client::builder()
            .with_rpc_url(self.rpc_url.clone())
            .with_private_key(self.signer.clone())
            .config_storage_layer(|c| c.inline_input_max_bytes(8192))
            .build()
            .await
            .map_err(|e| {
                warn!(
                    error = %e,
                    error_debug = ?e,
                    rpc_url = %self.rpc_url.origin().unicode_serialization(),
                    boundless_wallet = %self.signer.address(),
                    "failed to build Boundless client"
                );
                ProverError::Boundless(format!("failed to build client: {e}"))
            })?;

        debug!("Boundless client built successfully");

        let params = RequestParams::new()
            .with_program_url(self.verifier_program_url.clone())
            .map_err(|e| {
                warn!(
                    error = %e,
                    error_debug = ?e,
                    program_url = %self.verifier_program_url.origin().unicode_serialization(),
                    "invalid Boundless program URL"
                );
                ProverError::Boundless(format!("invalid program URL: {e}"))
            })?
            .with_stdin(input_bytes)
            .with_image_id(image_id)
            .with_requirements(
                RequirementParams::builder().predicate(Predicate::prefix_match(image_id, [])),
            );
        let params = self.apply_offer_config(params);

        Ok((client, params))
    }

    /// Computes the effective expiry timestamp from the current time and
    /// the prover's timeout, taking the minimum with the on-chain expiry
    /// if provided.
    fn effective_expiry(&self, on_chain_expiry: Option<u64>) -> u64 {
        let timeout_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_add(self.timeout.as_secs());
        on_chain_expiry.map_or(timeout_at, |e| e.min(timeout_at))
    }

    /// Returns `true` if the proof's attestation timestamp is within
    /// [`max_attestation_age`](Self::max_attestation_age) of the current
    /// wall-clock time. Returns `false` (stale) when the journal cannot
    /// be decoded, since an undecodable proof is unlikely to verify
    /// on-chain.
    fn is_journal_fresh(&self, proof: &AttestationProof) -> bool {
        let journal = match VerifierJournal::decode(&proof.output) {
            Ok(j) => j,
            Err(e) => {
                warn!(error = %e, "failed to decode VerifierJournal from recovered proof, treating as stale");
                return false;
            }
        };

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let age = Duration::from_millis(now_ms.saturating_sub(journal.timestamp));

        if age > self.max_attestation_age {
            info!(
                age_secs = age.as_secs(),
                max_age_secs = self.max_attestation_age.as_secs(),
                timestamp_ms = journal.timestamp,
                "recovered proof attestation is stale, skipping"
            );
            return false;
        }

        true
    }

    /// Acquires the submit lock, submits a proof request on-chain, then
    /// waits for fulfillment and fetches the set inclusion receipt.
    ///
    /// Shared between [`generate_proof`](AttestationProofProvider::generate_proof)
    /// and the fresh-submission tail of
    /// [`generate_proof_for_signer`](AttestationProofProvider::generate_proof_for_signer).
    async fn submit_and_wait(
        &self,
        client: &BoundlessClient,
        params: RequestParams,
    ) -> Result<AttestationProof> {
        let (request_id, expires_at) = {
            let _guard = self.submit_lock.lock().await;
            // Anchor `Offer.rampUpStart` to the current wall-clock as
            // late as possible — after any recovery scan, preflight,
            // and `submit_lock` contention — so the full configured
            // lock/timeout window is available to provers once the
            // request hits the chain. See `apply_bidding_start`.
            let params = self.apply_bidding_start(params);
            client.submit_onchain(params).await.map_err(|e| {
                warn!(
                    error = %e,
                    error_debug = ?e,
                    image_id = ?self.image_id,
                    boundless_wallet = %self.signer.address(),
                    "failed to submit Boundless proof request on-chain"
                );
                ProverError::Boundless(format!("failed to submit request: {e}"))
            })?
        };

        info!(
            request_id = %request_id,
            expires_at,
            "proof request submitted, waiting for fulfillment"
        );

        let effective_expiry = self.effective_expiry(Some(expires_at));
        debug!(
            effective_expiry,
            request_id = %request_id,
            poll_interval = ?self.poll_interval,
            "waiting for fulfillment with computed expiry"
        );

        self.wait_and_fetch(client, request_id, effective_expiry).await
    }
}

#[async_trait::async_trait]
impl AttestationProofProvider for BoundlessProver {
    /// # Cancellation
    ///
    /// Cooperatively honors `cancel` once, at the top of the method,
    /// before building the client/params. The build + submit +
    /// wait-for-fulfillment phases are not interrupted once entered;
    /// callers that need finer-grained cancellation should `select!`
    /// against the cancel token externally. Dropping the future
    /// mid-flight is safe because any submitted request remains
    /// discoverable on the next call via the deterministic request-id
    /// derivation (see the module docs on proof recovery).
    ///
    /// The per-probe cancel checks the doc previously referenced live
    /// in [`Self::generate_proof_for_signer`], which has its own
    /// recovery loop; this method has no probes to break between.
    async fn generate_proof(
        &self,
        attestation_bytes: &[u8],
        cancel: &CancellationToken,
    ) -> Result<AttestationProof> {
        if cancel.is_cancelled() {
            return Err(ProverError::Boundless("proof generation cancelled before start".into()));
        }
        let (client, params) = self.build_client_and_params(attestation_bytes).await?;
        self.submit_and_wait(&client, params).await
    }

    /// Generates a proof with deterministic request-ID recovery.
    ///
    /// Before submitting a new proof request, this method probes up to
    /// [`MAX_RECOVERY_ATTEMPTS`] deterministic request-ID slots derived
    /// from `signer_address` to find any in-flight or fulfilled proof
    /// from a previous instance. This allows the registrar to survive
    /// instance rotations without paying for duplicate proofs.
    ///
    /// Recovery outcomes per slot:
    /// - **`Locked`** — an in-flight proof is being worked on by a
    ///   Boundless prover. Resume polling for fulfillment.
    /// - **`Fulfilled`** — a previous instance's proof completed. Fetch
    ///   the receipt directly.
    /// - **`Expired`** — the slot was used but the proof expired. Skip
    ///   to the next attempt.
    /// - **`Unknown`** — the slot is unused. Submit a new request with
    ///   this deterministic ID.
    ///
    /// If recovery fails for any reason (RPC errors, receipt fetch
    /// failures, etc.), the method logs a warning and falls through to
    /// submit a fresh proof — the same graceful degradation as the
    /// non-recovery path.
    async fn generate_proof_for_signer(
        &self,
        attestation_bytes: &[u8],
        signer_address: Address,
        cancel: &CancellationToken,
    ) -> Result<AttestationProof> {
        if cancel.is_cancelled() {
            return Err(ProverError::Boundless("proof generation cancelled before start".into()));
        }
        let (client, params) = self.build_client_and_params(attestation_bytes).await?;

        let recovery_is_blocked = self
            .recovery_blocked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&signer_address);

        if recovery_is_blocked {
            info!(
                target_signer = %signer_address,
                "recovery blocked for signer, will skip recovered proofs"
            );
        }

        // Probe deterministic request-ID slots for recovery.
        let mut first_unknown_attempt: Option<u32> = None;
        for attempt in 0..self.max_recovery_attempts {
            // Check cancellation between probe slots. Already-locked
            // requests stay on-chain and remain recoverable on the next
            // call via the same deterministic index.
            if cancel.is_cancelled() {
                return Err(ProverError::Boundless(
                    "proof generation cancelled mid-recovery probe".into(),
                ));
            }
            let index = Self::derive_request_index(signer_address, attempt);
            // RequestId is keyed on the Boundless wallet (fee-payer,
            // `self.signer`), not the enclave signer (`signer_address`).
            // The index is derived from signer_address so that different
            // enclave signers occupy different slots.
            let rid = RequestId::new(self.signer.address(), index);
            let request_id: alloy_primitives::U256 = rid.into();

            debug!(
                attempt,
                index,
                request_id = %request_id,
                target_signer = %signer_address,
                "probing deterministic request-ID slot"
            );

            // NOTE: `get_status` is not exposed on `Client` directly;
            // we reach into the public `boundless_market` field. If a
            // future SDK release makes this field private, this will
            // fail at compile time — check whether `Client` gained a
            // `get_status` method and migrate accordingly.
            let status = match client.boundless_market.get_status(request_id, None).await {
                Ok(s) => s,
                Err(e) => {
                    // The same TOCTOU race that affects
                    // wait_for_request_fulfillment can also hit
                    // get_status: the request transitions from Locked
                    // to Fulfilled between the is_locked and
                    // requestDeadline calls, causing a
                    // RequestIsNotLocked revert. Treat this as
                    // evidence the request is in-flight/fulfilled and
                    // jump directly to wait_and_fetch — unless
                    // recovery is blocked.
                    if Self::is_request_not_locked_error(&e) {
                        if recovery_is_blocked {
                            debug!(
                                attempt,
                                request_id = %request_id,
                                target_signer = %signer_address,
                                "RequestIsNotLocked during scan, \
                                 skipping (recovery blocked)"
                            );
                            continue;
                        }
                        info!(
                            attempt,
                            request_id = %request_id,
                            target_signer = %signer_address,
                            "RequestIsNotLocked during recovery scan, \
                             treating as in-flight"
                        );
                        let effective_expiry = self.effective_expiry(None);
                        match self.wait_and_fetch(&client, request_id, effective_expiry).await {
                            Ok(proof) => return Ok(proof),
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    attempt,
                                    request_id = %request_id,
                                    target_signer = %signer_address,
                                    "recovery after TOCTOU failed, \
                                     falling through to fresh submission"
                                );
                                break;
                            }
                        }
                    }
                    warn!(
                        error = %e,
                        attempt,
                        request_id = %request_id,
                        target_signer = %signer_address,
                        "failed to query request status during recovery, \
                         falling through to fresh submission"
                    );
                    break;
                }
            };

            match status {
                RequestStatus::Locked => {
                    if recovery_is_blocked {
                        debug!(
                            attempt,
                            request_id = %request_id,
                            target_signer = %signer_address,
                            "slot is Locked, skipping (recovery blocked)"
                        );
                        continue;
                    }
                    info!(
                        attempt,
                        request_id = %request_id,
                        target_signer = %signer_address,
                        "recovered in-flight proof (Locked), resuming fulfillment poll"
                    );
                    let effective_expiry = self.effective_expiry(None);
                    match self.wait_and_fetch(&client, request_id, effective_expiry).await {
                        Ok(proof) => return Ok(proof),
                        Err(e) => {
                            warn!(
                                error = %e,
                                attempt,
                                request_id = %request_id,
                                target_signer = %signer_address,
                                "recovery poll failed, falling through to fresh submission"
                            );
                            break;
                        }
                    }
                }
                RequestStatus::Fulfilled => {
                    if recovery_is_blocked {
                        debug!(
                            attempt,
                            request_id = %request_id,
                            target_signer = %signer_address,
                            "slot is Fulfilled, skipping (recovery blocked)"
                        );
                        continue;
                    }
                    info!(
                        attempt,
                        request_id = %request_id,
                        target_signer = %signer_address,
                        "recovered fulfilled proof, fetching receipt"
                    );
                    match self.fetch_and_encode_receipt(&client, request_id).await {
                        Ok(proof) => {
                            if !self.is_journal_fresh(&proof) {
                                continue;
                            }
                            return Ok(proof);
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                attempt,
                                request_id = %request_id,
                                target_signer = %signer_address,
                                "recovery receipt fetch failed, \
                                 falling through to fresh submission"
                            );
                            break;
                        }
                    }
                }
                RequestStatus::Expired => {
                    // Note: expired slots are never reclaimed. After
                    // `max_recovery_attempts` consecutive expirations for
                    // a given signer (across any number of restarts), all
                    // deterministic slots will be permanently Expired and
                    // subsequent calls will fall back to random, non-
                    // recoverable request IDs. This is acceptable because
                    // repeated expirations indicate a systemic issue
                    // (misconfigured timeout, marketplace problems) that
                    // requires operator intervention regardless. The
                    // "falling back to random request ID" warning serves
                    // as the monitoring signal for this condition.
                    debug!(
                        attempt,
                        request_id = %request_id,
                        target_signer = %signer_address,
                        "slot expired, trying next attempt"
                    );
                    continue;
                }
                RequestStatus::Unknown => {
                    if first_unknown_attempt.is_none() {
                        first_unknown_attempt = Some(attempt);
                    }
                    debug!(
                        attempt,
                        request_id = %request_id,
                        target_signer = %signer_address,
                        "slot unused, will use for fresh submission if no recovery found"
                    );
                    // Continue scanning — a later slot might be Locked or
                    // Fulfilled from a previous instance that used a higher
                    // attempt counter.
                    continue;
                }
            }
        }

        // No recoverable proof found — submit a new request. If an unused
        // slot was found during the scan, use it as a deterministic ID so
        // future restarts can discover this proof. Otherwise (all slots
        // occupied/expired), fall through to SDK-generated random ID —
        // this request won't be recoverable but avoids colliding with an
        // existing request.
        let params = match first_unknown_attempt {
            Some(submit_attempt) => {
                let index = Self::derive_request_index(signer_address, submit_attempt);
                let rid = RequestId::new(self.signer.address(), index);
                let request_id_u256: alloy_primitives::U256 = rid.clone().into();
                info!(
                    attempt = submit_attempt,
                    index,
                    request_id = %request_id_u256,
                    target_signer = %signer_address,
                    "submitting with deterministic request ID"
                );
                params.with_request_id(rid)
            }
            None => {
                warn!(
                    target_signer = %signer_address,
                    max_attempts = self.max_recovery_attempts,
                    "all deterministic slots occupied, \
                     falling back to random request ID (non-recoverable)"
                );
                params
            }
        };

        self.submit_and_wait(&client, params).await
    }

    fn block_recovery_for_signer(&self, signer: Address) {
        info!(
            signer = %signer,
            "blocking proof recovery for signer after on-chain rejection"
        );
        self.recovery_blocked.lock().unwrap_or_else(|e| e.into_inner()).insert(signer);
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy_primitives::Address;
    use boundless_market::price_oracle::{Amount, Asset};
    use rstest::{fixture, rstest};

    use super::*;

    const TEST_RPC_URL: &str = "http://localhost:8545";
    const TEST_PROGRAM_URL: &str = "https://gateway.pinata.cloud/ipfs/bafybeitest";
    /// Well-known Hardhat/Anvil account #0 private key (not a real secret).
    const TEST_PRIVATE_KEY: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const TEST_IMAGE_ID: [u32; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    const TEST_POLL_INTERVAL: Duration = Duration::from_secs(5);
    const TEST_TIMEOUT: Duration = Duration::from_secs(300);
    const DEFAULT_TRUSTED_PREFIX: u8 = 1;
    const TEST_MAX_RECOVERY_ATTEMPTS: u32 = 5;
    const TEST_MIN_PRICE_ETH: &str = "0.01";
    const TEST_MAX_PRICE_ETH: &str = "0.03";
    const TEST_RAMP_UP_PERIOD_SECS: u32 = 30;
    const TEST_LOCK_TIMEOUT_SECS: u32 = 600;
    const TEST_NON_ZERO_BIDDING_START_DELAY_SECS: u64 = 120;

    const TEST_MAX_ATTESTATION_AGE: Duration = Duration::from_secs(3300);

    fn eth_amount(value: &str) -> Amount {
        Amount::parse(value, Some(Asset::ETH)).expect("valid ETH amount")
    }

    #[fixture]
    fn prover() -> BoundlessProver {
        BoundlessProver {
            rpc_url: Url::parse(TEST_RPC_URL).unwrap(),
            signer: PrivateKeySigner::from_str(TEST_PRIVATE_KEY).unwrap(),
            verifier_program_url: Url::parse(TEST_PROGRAM_URL).unwrap(),
            image_id: TEST_IMAGE_ID,
            poll_interval: TEST_POLL_INTERVAL,
            timeout: TEST_TIMEOUT,
            trusted_certs_prefix_len: DEFAULT_TRUSTED_PREFIX,
            max_recovery_attempts: TEST_MAX_RECOVERY_ATTEMPTS,
            max_attestation_age: TEST_MAX_ATTESTATION_AGE,
            offer_min_price: None,
            offer_max_price: None,
            offer_ramp_up_period_secs: None,
            offer_lock_timeout_secs: None,
            offer_bidding_start_delay_secs: 0,
            submit_lock: Arc::new(Mutex::new(())),
            recovery_blocked: Arc::new(std::sync::Mutex::new(HashSet::new())),
        }
    }

    // ── Construction ────────────────────────────────────────────────────

    #[rstest]
    fn struct_construction(prover: BoundlessProver) {
        let debug = format!("{prover:?}");
        assert!(debug.contains("BoundlessProver"));
    }

    // ── Field access ────────────────────────────────────────────────────

    #[rstest]
    fn fields_preserve_values(prover: BoundlessProver) {
        assert_eq!(prover.rpc_url, Url::parse(TEST_RPC_URL).unwrap());
        assert_eq!(
            prover.signer.address(),
            PrivateKeySigner::from_str(TEST_PRIVATE_KEY).unwrap().address()
        );
        assert_eq!(prover.verifier_program_url, Url::parse(TEST_PROGRAM_URL).unwrap());
        assert_eq!(prover.image_id, TEST_IMAGE_ID);
        assert_eq!(prover.poll_interval, TEST_POLL_INTERVAL);
        assert_eq!(prover.timeout, TEST_TIMEOUT);
        assert_eq!(prover.trusted_certs_prefix_len, DEFAULT_TRUSTED_PREFIX);
        assert_eq!(prover.max_recovery_attempts, TEST_MAX_RECOVERY_ATTEMPTS);
        assert!(prover.offer_min_price.is_none());
        assert!(prover.offer_max_price.is_none());
        assert!(prover.offer_ramp_up_period_secs.is_none());
        assert!(prover.offer_lock_timeout_secs.is_none());
        assert_eq!(prover.offer_bidding_start_delay_secs, 0);
    }

    /// With every Option-typed offer override unset, `apply_offer_config`
    /// leaves the corresponding offer fields unset so the Boundless SDK
    /// derives them from cycle count. `bidding_start` is intentionally
    /// **not** set here — it is anchored later by `apply_bidding_start`
    /// immediately before on-chain submission.
    #[rstest]
    fn apply_offer_config_defaults_to_sdk(prover: BoundlessProver) {
        let params = prover.apply_offer_config(RequestParams::new());

        assert!(params.offer.ramp_up_period.is_none());
        assert!(params.offer.min_price.is_none());
        assert!(params.offer.max_price.is_none());
        assert!(params.offer.lock_timeout.is_none());
        assert!(
            params.offer.bidding_start.is_none(),
            "bidding_start must be left unset by apply_offer_config and only \
             populated by apply_bidding_start at submission time"
        );
    }

    #[rstest]
    fn apply_offer_config_sets_explicit_prices(mut prover: BoundlessProver) {
        let min_price = eth_amount(TEST_MIN_PRICE_ETH);
        let max_price = eth_amount(TEST_MAX_PRICE_ETH);
        prover.offer_min_price = Some(min_price.clone());
        prover.offer_max_price = Some(max_price.clone());
        prover.offer_ramp_up_period_secs = Some(TEST_RAMP_UP_PERIOD_SECS);
        prover.offer_lock_timeout_secs = Some(TEST_LOCK_TIMEOUT_SECS);
        prover.offer_bidding_start_delay_secs = TEST_NON_ZERO_BIDDING_START_DELAY_SECS;

        let params = prover.apply_offer_config(RequestParams::new());

        assert_eq!(params.offer.min_price, Some(min_price));
        assert_eq!(params.offer.max_price, Some(max_price));
        assert_eq!(params.offer.ramp_up_period, Some(TEST_RAMP_UP_PERIOD_SECS));
        assert_eq!(params.offer.lock_timeout, Some(TEST_LOCK_TIMEOUT_SECS));
        assert!(
            params.offer.bidding_start.is_none(),
            "bidding_start is set by apply_bidding_start, not apply_offer_config"
        );
    }

    /// `lock_timeout` can be set independently of price/ramp fields.
    #[rstest]
    fn apply_offer_config_sets_lock_timeout_alone(mut prover: BoundlessProver) {
        prover.offer_lock_timeout_secs = Some(TEST_LOCK_TIMEOUT_SECS);

        let params = prover.apply_offer_config(RequestParams::new());

        assert_eq!(params.offer.lock_timeout, Some(TEST_LOCK_TIMEOUT_SECS));
        assert!(params.offer.ramp_up_period.is_none());
        assert!(params.offer.min_price.is_none());
        assert!(params.offer.max_price.is_none());
        assert!(params.offer.bidding_start.is_none());
    }

    /// `apply_bidding_start` with the default (`0`) delay sets
    /// `bidding_start` to the current wall-clock at call time.
    #[rstest]
    fn apply_bidding_start_defaults_to_now(prover: BoundlessProver) {
        let before = BoundlessProver::now_unix_secs();
        let params = prover.apply_bidding_start(RequestParams::new());
        let after = BoundlessProver::now_unix_secs();

        let bidding_start = params.offer.bidding_start.expect("bidding_start set");
        assert!(
            (before..=after).contains(&bidding_start),
            "bidding_start {bidding_start} outside [{before}, {after}]"
        );
    }

    /// Non-zero `bidding_start_delay` is offset from the current clock
    /// at call time (per-request, not captured at startup), so the
    /// resulting timestamp is always in the configured window.
    #[rstest]
    fn apply_bidding_start_uses_current_clock(mut prover: BoundlessProver) {
        prover.offer_bidding_start_delay_secs = TEST_NON_ZERO_BIDDING_START_DELAY_SECS;

        let before = BoundlessProver::now_unix_secs();
        let params = prover.apply_bidding_start(RequestParams::new());
        let after = BoundlessProver::now_unix_secs();

        let bidding_start = params.offer.bidding_start.expect("bidding_start set");
        assert!(
            (before.saturating_add(TEST_NON_ZERO_BIDDING_START_DELAY_SECS)
                ..=after.saturating_add(TEST_NON_ZERO_BIDDING_START_DELAY_SECS))
                .contains(&bidding_start),
            "bidding_start {bidding_start} outside expected window"
        );
    }

    /// `apply_bidding_start` preserves all other offer fields set by
    /// `apply_offer_config` — it mutates `bidding_start` in place
    /// rather than replacing the offer.
    #[rstest]
    fn apply_bidding_start_preserves_other_offer_fields(mut prover: BoundlessProver) {
        let min_price = eth_amount(TEST_MIN_PRICE_ETH);
        let max_price = eth_amount(TEST_MAX_PRICE_ETH);
        prover.offer_min_price = Some(min_price.clone());
        prover.offer_max_price = Some(max_price.clone());
        prover.offer_ramp_up_period_secs = Some(TEST_RAMP_UP_PERIOD_SECS);
        prover.offer_lock_timeout_secs = Some(TEST_LOCK_TIMEOUT_SECS);

        let params = prover.apply_offer_config(RequestParams::new());
        let params = prover.apply_bidding_start(params);

        assert_eq!(params.offer.min_price, Some(min_price));
        assert_eq!(params.offer.max_price, Some(max_price));
        assert_eq!(params.offer.ramp_up_period, Some(TEST_RAMP_UP_PERIOD_SECS));
        assert_eq!(params.offer.lock_timeout, Some(TEST_LOCK_TIMEOUT_SECS));
        assert!(params.offer.bidding_start.is_some());
    }

    // ── Clone ───────────────────────────────────────────────────────────

    #[rstest]
    fn clone_preserves_values(prover: BoundlessProver) {
        let cloned = prover.clone();
        assert_eq!(cloned.rpc_url, prover.rpc_url);
        assert_eq!(cloned.signer.address(), prover.signer.address());
        assert_eq!(cloned.image_id, prover.image_id);
        assert_eq!(cloned.timeout, prover.timeout);
    }

    // ── Debug redaction ──────────────────────────────────────────────────

    #[rstest]
    fn debug_redacts_rpc_url_path() {
        let api_key = "s3cret-api-key-12345";
        let rpc_with_key = format!("https://mainnet.infura.io/v3/{api_key}");
        let mut prover = prover();
        prover.rpc_url = Url::parse(&rpc_with_key).unwrap();

        let debug = format!("{prover:?}");
        assert!(!debug.contains(api_key), "RPC URL path (API key) must not appear in Debug output");
        assert!(debug.contains("mainnet.infura.io"), "RPC host should still be visible");
    }

    #[rstest]
    fn debug_shows_address_not_key(prover: BoundlessProver) {
        let debug = format!("{prover:?}");
        let expected_addr = format!("{:?}", prover.signer.address());
        assert!(
            debug.contains(&expected_addr),
            "Debug should show the signer address, got: {debug}"
        );
        assert!(
            !debug.contains(TEST_PRIVATE_KEY),
            "raw private key must not appear in Debug output"
        );
    }

    // ── derive_request_index ──────────────────────────────────────────────

    /// Synthetic signer addresses for deterministic-ID tests.
    const SIGNER_A: Address = Address::repeat_byte(0xAA);
    const SIGNER_B: Address = Address::repeat_byte(0xBB);

    /// Computes the expected index via the reference algorithm so tests
    /// can assert against it without duplicating the constant `24`.
    fn expected_index(addr: Address, attempt: u32) -> u32 {
        let mut buf = [0u8; 24];
        buf[..20].copy_from_slice(addr.as_slice());
        buf[20..].copy_from_slice(&attempt.to_be_bytes());
        let hash = keccak256(buf);
        u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]])
    }

    #[rstest]
    #[case::typical_address(SIGNER_A, 0)]
    #[case::zero_address(Address::ZERO, 0)]
    #[case::max_attempt(SIGNER_A, u32::MAX)]
    #[case::zero_address_max_attempt(Address::ZERO, u32::MAX)]
    fn derive_index_is_deterministic(#[case] addr: Address, #[case] attempt: u32) {
        let a = BoundlessProver::derive_request_index(addr, attempt);
        let b = BoundlessProver::derive_request_index(addr, attempt);
        assert_eq!(a, b, "same inputs must produce the same index");
    }

    #[rstest]
    #[case::typical_address(SIGNER_A)]
    #[case::zero_address(Address::ZERO)]
    fn derive_index_varies_with_attempt(#[case] addr: Address) {
        let indices: Vec<u32> = (0..TEST_MAX_RECOVERY_ATTEMPTS)
            .map(|a| BoundlessProver::derive_request_index(addr, a))
            .collect();
        // All indices should be distinct (collision probability is negligible
        // for 5 values in a 2^32 space).
        let unique: std::collections::HashSet<u32> = indices.iter().copied().collect();
        assert_eq!(unique.len(), indices.len(), "each attempt should produce a distinct index");
    }

    #[rstest]
    #[case::distinct_addresses(SIGNER_A, SIGNER_B, 0)]
    #[case::zero_vs_nonzero(Address::ZERO, SIGNER_A, 0)]
    #[case::same_attempt_different_addr(SIGNER_A, SIGNER_B, 3)]
    fn derive_index_varies_with_address(
        #[case] addr_a: Address,
        #[case] addr_b: Address,
        #[case] attempt: u32,
    ) {
        let a = BoundlessProver::derive_request_index(addr_a, attempt);
        let b = BoundlessProver::derive_request_index(addr_b, attempt);
        assert_ne!(a, b, "different addresses should produce different indices");
    }

    /// Verifies the implementation matches a manual `keccak256(addr || attempt)`
    /// computation across multiple (address, attempt) pairs.
    #[rstest]
    #[case::typical(SIGNER_A, 0)]
    #[case::nonzero_attempt(SIGNER_B, 7)]
    #[case::zero_address(Address::ZERO, 1)]
    #[case::max_attempt(SIGNER_A, u32::MAX)]
    fn derive_index_matches_manual_keccak(#[case] addr: Address, #[case] attempt: u32) {
        assert_eq!(
            BoundlessProver::derive_request_index(addr, attempt),
            expected_index(addr, attempt)
        );
    }

    // ── effective_expiry ────────────────────────────────────────────────

    /// When an on-chain expiry is provided and is sooner than the
    /// timeout, the effective expiry equals the on-chain value.
    #[rstest]
    fn effective_expiry_picks_on_chain_when_sooner(prover: BoundlessProver) {
        // Use a very near on-chain expiry (1 second from now).
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let on_chain = now + 1;
        let result = prover.effective_expiry(Some(on_chain));
        assert_eq!(result, on_chain, "should pick the nearer on-chain expiry");
    }

    /// When no on-chain expiry is provided, the effective expiry is
    /// `now + timeout`.
    #[rstest]
    fn effective_expiry_uses_timeout_when_none(prover: BoundlessProver) {
        let before =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let result = prover.effective_expiry(None);
        let after =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        // Result should be in [before + timeout, after + timeout].
        let timeout_secs = prover.timeout.as_secs();
        assert!(
            result >= before + timeout_secs && result <= after + timeout_secs,
            "expected ~now + {timeout_secs}, got {result} (now ≈ {before})"
        );
    }

    /// When the on-chain expiry is far in the future, the effective
    /// expiry is clamped to `now + timeout`.
    #[rstest]
    fn effective_expiry_clamps_to_timeout(prover: BoundlessProver) {
        let far_future = u64::MAX;
        let before =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let result = prover.effective_expiry(Some(far_future));
        let timeout_secs = prover.timeout.as_secs();
        // Should be clamped to approximately now + timeout, not u64::MAX.
        assert!(
            result <= before + timeout_secs + 1,
            "expected ≤ now + {timeout_secs}, got {result}"
        );
    }

    // ── is_request_not_locked_error ─────────────────────────────────────
    //
    // These tests construct the *real* Boundless SDK error types that the
    // `RequestIsNotLocked` Solidity custom error produces in production.
    // If a `boundless-market` upgrade changes the Display/Debug formatting
    // of `ClientError` → `MarketError` → `TxnErr` →
    // `IBoundlessMarketErrors::RequestIsNotLocked`, these tests will fail
    // and alert us that the string-matching needle needs updating.

    mod request_not_locked {
        use alloy_primitives::{U256, uint};
        use boundless_market::{
            client::ClientError,
            contracts::{
                IBoundlessMarket::{self, IBoundlessMarketErrors},
                TxnErr,
                boundless_market::MarketError,
            },
        };

        use super::*;

        /// Arbitrary request ID used in error construction.
        const TEST_REQUEST_ID: U256 = uint!(42_U256);

        /// Build a `ClientError` wrapping `RequestIsNotLocked` through the
        /// **production** path: `TxnErr` → `anyhow::Error` →
        /// `MarketError::Error` → `ClientError::MarketError`.
        fn production_path_error() -> ClientError {
            let revert =
                IBoundlessMarketErrors::RequestIsNotLocked(IBoundlessMarket::RequestIsNotLocked {
                    requestId: TEST_REQUEST_ID,
                });
            let txn_err = TxnErr::BoundlessMarketErr(revert);
            // Production wraps TxnErr in anyhow::Error, then into MarketError::Error.
            let market_err = MarketError::Error(anyhow::Error::from(txn_err));
            ClientError::MarketError(market_err)
        }

        /// Build a `ClientError` wrapping `RequestIsNotLocked` through the
        /// **direct** path: `TxnErr` → `MarketError::TxnError` →
        /// `ClientError::MarketError`.
        fn direct_path_error() -> ClientError {
            let revert =
                IBoundlessMarketErrors::RequestIsNotLocked(IBoundlessMarket::RequestIsNotLocked {
                    requestId: TEST_REQUEST_ID,
                });
            let txn_err = TxnErr::BoundlessMarketErr(revert);
            let market_err = MarketError::TxnError(txn_err);
            ClientError::MarketError(market_err)
        }

        /// Build a `ClientError` for a **different** Solidity error
        /// (`RequestIsLocked`) to verify we don't false-positive.
        fn different_revert_error() -> ClientError {
            let revert =
                IBoundlessMarketErrors::RequestIsLocked(IBoundlessMarket::RequestIsLocked {
                    requestId: TEST_REQUEST_ID,
                });
            let txn_err = TxnErr::BoundlessMarketErr(revert);
            let market_err = MarketError::Error(anyhow::Error::from(txn_err));
            ClientError::MarketError(market_err)
        }

        /// Production error chain (anyhow-wrapped) matches.
        #[rstest]
        fn matches_production_path() {
            let err = production_path_error();
            assert!(
                BoundlessProver::is_request_not_locked_error(&err),
                "should detect RequestIsNotLocked through production error chain. \
                 Display: {err}, Debug: {err:?}"
            );
        }

        /// Direct error chain (`MarketError::TxnError`) matches.
        #[rstest]
        fn matches_direct_path() {
            let err = direct_path_error();
            assert!(
                BoundlessProver::is_request_not_locked_error(&err),
                "should detect RequestIsNotLocked through direct error chain. \
                 Display: {err}, Debug: {err:?}"
            );
        }

        /// A different revert (`RequestIsLocked`) must NOT match.
        #[rstest]
        fn rejects_different_revert() {
            let err = different_revert_error();
            assert!(
                !BoundlessProver::is_request_not_locked_error(&err),
                "should NOT match RequestIsLocked (different error). \
                 Display: {err}, Debug: {err:?}"
            );
        }

        /// Plain `std::io::Error` must NOT match.
        #[rstest]
        fn rejects_unrelated_error() {
            let err = std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out");
            assert!(
                !BoundlessProver::is_request_not_locked_error(&err),
                "should NOT match an unrelated I/O error"
            );
        }
    }
}
