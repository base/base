//! Boundless marketplace TDX attestation proving backend.

use std::{collections::HashSet, fmt, sync::Arc, time::Duration};

use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_sol_types::SolValue;
use async_trait::async_trait;
use base_proof_tee_attestation::{
    TeeAttestationKind, TeeAttestationProof, TeeAttestationProofProvider,
};
use base_proof_tee_tdx_verifier::TDXVerifierJournal;
use boundless_market::{
    Client, NotProvided,
    alloy::providers::DynProvider,
    alloy::signers::local::PrivateKeySigner,
    contracts::{Predicate, RequestId, RequestStatus},
    request_builder::{RequestParams, RequirementParams, StandardRequestBuilder},
};
use risc0_zkvm::sha::Digest;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use url::Url;

use crate::{ProverError, RecoveredProofPolicy, Result, TdxAttestationProverInput};

/// Concrete Boundless client type used by the TDX prover.
type BoundlessClient = Client<
    DynProvider,
    NotProvided,
    boundless_market::StandardDownloader,
    StandardRequestBuilder<DynProvider, NotProvided, boundless_market::StandardDownloader>,
    PrivateKeySigner,
>;

/// Attestation prover using the Boundless marketplace for RISC Zero proofs.
#[derive(Clone)]
pub struct BoundlessProver {
    /// Ethereum RPC URL for the Boundless settlement chain.
    pub rpc_url: Url,
    /// Signer for Boundless Network proving fees.
    pub signer: PrivateKeySigner,
    /// HTTP(S) URL where the TDX verifier guest ELF is hosted.
    pub verifier_program_url: Url,
    /// Expected image ID of the TDX verifier guest.
    pub image_id: [u32; 8],
    /// Interval between fulfillment status checks.
    pub poll_interval: Duration,
    /// Maximum time to wait for proof fulfillment.
    pub timeout: Duration,
    /// Maximum number of deterministic request-ID slots to probe.
    pub max_recovery_attempts: u32,
    /// Freshness policy for recovered proof journals.
    pub recovered_proof_policy: RecoveredProofPolicy,
    /// Serializes Boundless on-chain request submission by wallet nonce.
    pub submit_lock: Arc<Mutex<()>>,
    /// Signers whose recovered proofs have already been rejected on-chain.
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
            .field("max_recovery_attempts", &self.max_recovery_attempts)
            .field("recovered_proof_policy", &self.recovered_proof_policy)
            .finish()
    }
}

impl BoundlessProver {
    /// Derives a deterministic Boundless request index from signer and attempt.
    pub fn derive_request_index(signer_address: Address, attempt: u32) -> u32 {
        let mut buf = [0u8; 24];
        buf[..20].copy_from_slice(signer_address.as_slice());
        buf[20..].copy_from_slice(&attempt.to_be_bytes());
        let hash = keccak256(buf);
        u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]])
    }

    /// Checks whether a Boundless error is the `RequestIsNotLocked` race.
    fn is_request_not_locked_error(error: &dyn std::error::Error) -> bool {
        const NEEDLE: &str = "requestisnotlocked";
        format!("{error} {error:?}").to_ascii_lowercase().contains(NEEDLE)
    }

    /// Builds the Boundless client and request params for a TDX verifier input.
    async fn build_client_and_params(
        &self,
        input: &TdxAttestationProverInput,
    ) -> Result<(BoundlessClient, RequestParams)> {
        let input_bytes = input.encode();
        let image_id = Digest::from(self.image_id);

        info!(
            image_id = ?self.image_id,
            input_len = input_bytes.len(),
            quote_timestamp_millis = input.quote_timestamp_millis(),
            rpc_url = %self.rpc_url.origin().unicode_serialization(),
            boundless_wallet = %self.signer.address(),
            program_url = %self.verifier_program_url.origin().unicode_serialization(),
            timeout = ?self.timeout,
            poll_interval = ?self.poll_interval,
            "building Boundless TDX client and request params"
        );

        let client = Client::builder()
            .with_rpc_url(self.rpc_url.clone())
            .with_private_key(self.signer.clone())
            .config_storage_layer(|config| config.inline_input_max_bytes(None::<usize>))
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

        Ok((client, params))
    }

    /// Computes the proof fulfillment expiry used while polling Boundless.
    fn effective_expiry(&self, on_chain_expiry: Option<u64>) -> u64 {
        let timeout_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_add(self.timeout.as_secs());
        on_chain_expiry.map_or(timeout_at, |expiry| expiry.min(timeout_at))
    }

    /// Fetches a fulfilled set-inclusion receipt and encodes it for `TDXVerifier`.
    async fn fetch_and_encode_receipt(
        &self,
        client: &BoundlessClient,
        request_id: alloy_primitives::U256,
    ) -> Result<TeeAttestationProof> {
        let image_id_bytes: [u8; 32] = Digest::from(self.image_id).into();
        let image_id_b256 = B256::from(image_id_bytes);

        let (journal, receipt) = client
            .fetch_set_inclusion_receipt(request_id, image_id_b256, None, None)
            .await
            .map_err(|e| {
                warn!(
                    error = %e,
                    error_debug = ?e,
                    request_id = %request_id,
                    image_id = ?self.image_id,
                    "failed to fetch set inclusion receipt"
                );
                ProverError::Boundless(format!("failed to fetch set inclusion receipt: {e}"))
            })?;
        let encoded_seal = receipt.abi_encode_seal().map_err(|e| {
            warn!(
                error = %e,
                error_debug = ?e,
                request_id = %request_id,
                "failed to ABI-encode set inclusion seal"
            );
            ProverError::Boundless(format!("failed to encode set inclusion seal: {e}"))
        })?;

        Ok(TeeAttestationProof {
            kind: TeeAttestationKind::Tdx,
            output: journal,
            proof_bytes: Bytes::from(encoded_seal),
        })
    }

    /// Waits for fulfillment and fetches the encoded receipt.
    async fn wait_and_fetch(
        &self,
        client: &BoundlessClient,
        request_id: alloy_primitives::U256,
        effective_expiry: u64,
    ) -> Result<TeeAttestationProof> {
        const MAX_RACE_RETRIES: u32 = 3;
        let mut race_retries = 0;
        loop {
            match client
                .wait_for_request_fulfillment(request_id, self.poll_interval, effective_expiry)
                .await
            {
                Ok(_) => break,
                Err(e)
                    if Self::is_request_not_locked_error(&e) && race_retries < MAX_RACE_RETRIES =>
                {
                    race_retries += 1;
                    warn!(
                        error = %e,
                        request_id = %request_id,
                        retry = race_retries,
                        max_retries = MAX_RACE_RETRIES,
                        "RequestIsNotLocked race condition, retrying fulfillment poll"
                    );
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        error_debug = ?e,
                        request_id = %request_id,
                        effective_expiry,
                        "proof fulfillment failed"
                    );
                    return Err(ProverError::Boundless(format!("fulfillment failed: {e}")));
                }
            }
        }

        self.fetch_and_encode_receipt(client, request_id).await
    }

    /// Submits a fresh proof request and waits for fulfillment.
    async fn submit_and_wait(
        &self,
        client: &BoundlessClient,
        params: RequestParams,
    ) -> Result<TeeAttestationProof> {
        let (request_id, expires_at) = {
            let _guard = self.submit_lock.lock().await;
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

        info!(request_id = %request_id, expires_at, "proof request submitted");
        self.wait_and_fetch(client, request_id, self.effective_expiry(Some(expires_at))).await
    }

    /// Returns true when a recovered proof targets the signer and is fresh enough.
    fn recovered_proof_is_usable(
        &self,
        proof: &TeeAttestationProof,
        signer_address: Address,
    ) -> bool {
        let Ok(journal) = <TDXVerifierJournal as SolValue>::abi_decode_validate(&proof.output)
        else {
            info!("recovered TDX proof journal is malformed, skipping");
            return false;
        };

        if journal.signer != signer_address {
            warn!(
                journal_signer = %journal.signer,
                target_signer = %signer_address,
                "recovered TDX proof signer mismatch, skipping"
            );
            return false;
        }

        if self.recovered_proof_policy.journal_is_fresh(&journal) {
            return true;
        }
        info!(
            max_recovered_quote_age_secs =
                self.recovered_proof_policy.max_recovered_quote_age.as_secs(),
            quote_timestamp_millis = journal.timestamp,
            target_signer = %signer_address,
            "recovered TDX proof quote timestamp is too old, skipping"
        );
        false
    }
}

#[async_trait]
impl TeeAttestationProofProvider for BoundlessProver {
    async fn generate_proof_for_signer(
        &self,
        attestation_bytes: &[u8],
        signer_address: Address,
    ) -> base_proof_tee_attestation::Result<TeeAttestationProof> {
        let input =
            TdxAttestationProverInput::decode_for_signer(attestation_bytes, signer_address)?;

        let (client, params) = self.build_client_and_params(&input).await?;
        let recovery_is_blocked = self
            .recovery_blocked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&signer_address);

        let loop_expiry = self.effective_expiry(None);
        let mut first_unknown_attempt = None;
        for attempt in 0..self.max_recovery_attempts {
            let index = Self::derive_request_index(signer_address, attempt);
            let request_id = RequestId::new(self.signer.address(), index);
            let request_id_u256: alloy_primitives::U256 = request_id.into();

            debug!(
                attempt,
                index,
                request_id = %request_id_u256,
                target_signer = %signer_address,
                "probing deterministic TDX request-ID slot"
            );

            let proof = match client.boundless_market.get_status(request_id_u256, None).await {
                Ok(RequestStatus::Locked) if !recovery_is_blocked => {
                    self.wait_and_fetch(&client, request_id_u256, loop_expiry).await
                }
                Ok(RequestStatus::Fulfilled) if !recovery_is_blocked => {
                    self.fetch_and_encode_receipt(&client, request_id_u256).await
                }
                Ok(RequestStatus::Locked | RequestStatus::Fulfilled | RequestStatus::Expired) => {
                    continue;
                }
                Ok(RequestStatus::Unknown) => {
                    if first_unknown_attempt.is_none() {
                        first_unknown_attempt = Some(attempt);
                    }
                    continue;
                }
                Err(e) => {
                    if Self::is_request_not_locked_error(&e) && !recovery_is_blocked {
                        self.wait_and_fetch(&client, request_id_u256, loop_expiry).await
                    } else {
                        warn!(
                            error = %e,
                            error_debug = ?e,
                            attempt,
                            request_id = %request_id_u256,
                            target_signer = %signer_address,
                            "failed to query TDX request status during recovery"
                        );
                        break;
                    }
                }
            };

            let Ok(proof) = proof else {
                break;
            };
            if self.recovered_proof_is_usable(&proof, signer_address) {
                return Ok(proof);
            }
        }

        let params = match first_unknown_attempt {
            Some(attempt) => {
                let index = Self::derive_request_index(signer_address, attempt);
                let request_id = RequestId::new(self.signer.address(), index);
                params.with_request_id(request_id)
            }
            None => params,
        };

        Ok(self.submit_and_wait(&client, params).await?)
    }

    fn block_recovery_for_signer(&self, signer: Address) {
        info!(signer = %signer, "blocking TDX proof recovery for signer");
        self.recovery_blocked.lock().unwrap_or_else(|e| e.into_inner()).insert(signer);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        str::FromStr,
        time::{SystemTime, UNIX_EPOCH},
    };

    use base_proof_tee_tdx_verifier::{TDXTcbStatus, TDXVerificationResult};
    use rstest::{fixture, rstest};

    use super::*;

    const TEST_RPC_URL: &str = "http://localhost:8545";
    const TEST_PROGRAM_URL: &str = "https://gateway.pinata.cloud/ipfs/bafybeitdx";
    const TEST_PRIVATE_KEY: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const TEST_IMAGE_ID: [u32; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    #[fixture]
    fn prover() -> BoundlessProver {
        BoundlessProver {
            rpc_url: Url::parse(TEST_RPC_URL).unwrap(),
            signer: PrivateKeySigner::from_str(TEST_PRIVATE_KEY).unwrap(),
            verifier_program_url: Url::parse(TEST_PROGRAM_URL).unwrap(),
            image_id: TEST_IMAGE_ID,
            poll_interval: Duration::from_secs(5),
            timeout: Duration::from_secs(300),
            max_recovery_attempts: 5,
            recovered_proof_policy: RecoveredProofPolicy::new(Duration::from_secs(300)),
            submit_lock: Arc::new(Mutex::new(())),
            recovery_blocked: Arc::new(std::sync::Mutex::new(HashSet::new())),
        }
    }

    fn now_millis() -> u64 {
        u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
    }

    fn proof_for_signer(signer: Address) -> TeeAttestationProof {
        let journal = TDXVerifierJournal {
            result: TDXVerificationResult::Success,
            tcbStatus: TDXTcbStatus::UpToDate,
            timestamp: now_millis(),
            collateralExpiration: 1_711_222_222,
            rootCaHash: B256::repeat_byte(0x11),
            pckCertHash: B256::repeat_byte(0x22),
            tcbInfoHash: B256::repeat_byte(0x33),
            qeIdentityHash: B256::repeat_byte(0x44),
            publicKey: Bytes::from(vec![0x04; 65]),
            signer,
            imageHash: B256::repeat_byte(0x55),
            mrTdHash: B256::repeat_byte(0x66),
            reportDataPrefix: B256::repeat_byte(0x77),
            reportDataSuffix: B256::repeat_byte(0x88),
        };
        TeeAttestationProof {
            kind: TeeAttestationKind::Tdx,
            output: Bytes::from(SolValue::abi_encode(&journal)),
            proof_bytes: Bytes::from_static(b"proof"),
        }
    }

    #[rstest]
    fn derive_index_matches_manual_keccak() {
        let signer = Address::repeat_byte(0xAA);

        assert_eq!(BoundlessProver::derive_request_index(signer, 7), 0xF982_1D96);
    }

    #[rstest]
    fn recovered_proof_with_matching_signer_is_usable(prover: BoundlessProver) {
        let signer = Address::repeat_byte(0x11);

        assert!(prover.recovered_proof_is_usable(&proof_for_signer(signer), signer));
    }

    #[rstest]
    fn recovered_proof_with_different_signer_is_skipped(prover: BoundlessProver) {
        let target_signer = Address::repeat_byte(0x11);
        let journal_signer = Address::repeat_byte(0x22);

        assert!(
            !prover.recovered_proof_is_usable(&proof_for_signer(journal_signer), target_signer)
        );
    }

    #[rstest]
    fn debug_redacts_url_paths(prover: BoundlessProver) {
        let debug = format!("{prover:?}");

        assert!(debug.contains("localhost"));
        assert!(!debug.contains("bafybeitdx"));
        assert!(!debug.contains(TEST_PRIVATE_KEY));
    }
}
