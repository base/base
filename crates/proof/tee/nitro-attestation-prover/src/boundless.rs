//! [`BoundlessProver`] — proving backend using the Boundless marketplace.
//!
//! Submits proof requests to the Boundless decentralised proving marketplace
//! and polls for fulfillment with a configurable timeout.

use std::{fmt, time::Duration};

use alloy_primitives::{B256, Bytes};
use alloy_signer_local::PrivateKeySigner;
use base_proof_tee_nitro_verifier::VerifierInput;
use boundless_market::{
    Client,
    contracts::Predicate,
    request_builder::{RequestParams, RequirementParams},
};
use risc0_zkvm::sha::Digest;
use tracing::{debug, info, warn};
use url::Url;

use crate::{AttestationProof, AttestationProofProvider, ProverError, Result};

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
}

impl fmt::Debug for BoundlessProver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundlessProver")
            .field("rpc_url", &self.rpc_url.origin().unicode_serialization())
            .field("signer", &self.signer.address())
            .field("verifier_program_url", &self.verifier_program_url)
            .field("image_id", &self.image_id)
            .field("poll_interval", &self.poll_interval)
            .field("timeout", &self.timeout)
            .field("trusted_certs_prefix_len", &self.trusted_certs_prefix_len)
            .finish()
    }
}

#[async_trait::async_trait]
impl AttestationProofProvider for BoundlessProver {
    async fn generate_proof(&self, attestation_bytes: &[u8]) -> Result<AttestationProof> {
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
            signer_address = %self.signer.address(),
            program_url = %self.verifier_program_url,
            timeout = ?self.timeout,
            poll_interval = ?self.poll_interval,
            trusted_certs_prefix_len = self.trusted_certs_prefix_len,
            "submitting proof request to Boundless"
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
                    signer_address = %self.signer.address(),
                    "failed to build Boundless client"
                );
                ProverError::Boundless(format!("failed to build client: {e}"))
            })?;

        debug!("Boundless client built successfully");

        // Build request parameters: program URL + stdin input + predicate.
        let params = RequestParams::new()
            .with_program_url(self.verifier_program_url.clone())
            .map_err(|e| {
                warn!(
                    error = %e,
                    error_debug = ?e,
                    program_url = %self.verifier_program_url,
                    "invalid Boundless program URL"
                );
                ProverError::Boundless(format!("invalid program URL: {e}"))
            })?
            .with_stdin(input_bytes)
            .with_image_id(image_id)
            .with_requirements(
                RequirementParams::builder().predicate(Predicate::prefix_match(image_id, [])),
            );

        let (request_id, expires_at) = client.submit_onchain(params).await.map_err(|e| {
            warn!(
                error = %e,
                    error_debug = ?e,
                    image_id = ?self.image_id,
                    signer_address = %self.signer.address(),
                "failed to submit Boundless proof request on-chain"
            );
            ProverError::Boundless(format!("failed to submit request: {e}"))
        })?;

        info!(
            request_id = %request_id,
            expires_at,
            "proof request submitted, waiting for fulfillment"
        );

        // Compute the expiry from timeout: pick the sooner of expires_at and
        // now + timeout.
        let timeout_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_add(self.timeout.as_secs());
        let effective_expiry = expires_at.min(timeout_at);

        debug!(
            timeout_at,
            effective_expiry,
            request_id = %request_id,
            poll_interval = ?self.poll_interval,
            "waiting for fulfillment with computed expiry"
        );

        // Wait for marketplace fulfillment (prover completes the proof).
        let _fulfillment = client
            .wait_for_request_fulfillment(request_id, self.poll_interval, effective_expiry)
            .await
            .map_err(|e| {
                warn!(
                    error = %e,
                    error_debug = ?e,
                    request_id = %request_id,
                    effective_expiry,
                    timeout = ?self.timeout,
                    poll_interval = ?self.poll_interval,
                    "proof fulfillment failed"
                );
                ProverError::Boundless(format!("fulfillment failed: {e}"))
            })?;

        info!(request_id = %request_id, "fulfillment confirmed, fetching set inclusion receipt");

        // Fetch the set inclusion receipt, which contains the Merkle inclusion
        // path and root Groth16 proof needed for on-chain verification.
        // The raw fulfillment.seal is a marketplace seal — NOT an
        // independently-verifiable proof. The on-chain NitroEnclaveVerifier
        // routes proofs by the first 4 bytes (selector) to either a Groth16
        // verifier or a SetVerifier, so we must encode the seal correctly.
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

        // ABI-encode the seal: 4-byte selector + ABI-encoded Seal struct
        // (Merkle path + root Groth16 seal). This is the format expected by
        // the on-chain RiscZeroSetVerifier.
        let encoded_seal = receipt.abi_encode_seal().map_err(|e| {
            warn!(
                error = %e,
                error_debug = ?e,
                request_id = %request_id,
                "failed to ABI-encode set inclusion seal"
            );
            ProverError::Boundless(format!("failed to encode set inclusion seal: {e}"))
        })?;

        let output = Bytes::copy_from_slice(&journal);
        let proof_bytes = Bytes::from(encoded_seal);

        info!(
            request_id = %request_id,
            journal_len = output.len(),
            seal_len = proof_bytes.len(),
            "set inclusion receipt fetched and seal encoded successfully"
        );

        Ok(AttestationProof { output, proof_bytes })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

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
}
