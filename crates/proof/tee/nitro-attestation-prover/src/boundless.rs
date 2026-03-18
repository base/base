//! [`BoundlessProver`] — proving backend using the Boundless marketplace.
//!
//! Submits proof requests to the Boundless decentralised proving marketplace
//! and polls for fulfillment with a configurable timeout.

use std::{fmt, time::Duration};

use alloy_primitives::{Bytes, U256};
use alloy_signer_local::PrivateKeySigner;
use base_proof_tee_nitro_verifier::VerifierInput;
use boundless_market::{
    Client,
    contracts::Predicate,
    request_builder::{OfferParams, RequestParams, RequirementParams},
};
use risc0_zkvm::sha::Digest;
use tracing::{info, warn};
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
    /// URL (IPFS or HTTP) where the guest ELF is hosted.
    pub verifier_program_url: Url,
    /// Expected image ID of the guest program.
    pub image_id: [u32; 8],
    /// Maximum price (in wei) willing to pay for proving.
    pub max_price: u64,
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
            .field("max_price", &self.max_price)
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
            "submitting proof request to Boundless"
        );

        let client = Client::builder()
            .with_rpc_url(self.rpc_url.clone())
            .with_private_key(self.signer.clone())
            .build()
            .await
            .map_err(|e| ProverError::Boundless(format!("failed to build client: {e}")))?;

        // Build request parameters: program URL + stdin input + predicate.
        let params = RequestParams::new()
            .with_program_url(self.verifier_program_url.clone())
            .map_err(|e| ProverError::Boundless(format!("invalid program URL: {e}")))?
            .with_stdin(input_bytes)
            .with_image_id(image_id)
            .with_requirements(
                RequirementParams::builder().predicate(Predicate::prefix_match(image_id, [])),
            )
            .with_offer(OfferParams::builder().max_price(U256::from(self.max_price)));

        let (request_id, expires_at) = client
            .submit_onchain(params)
            .await
            .map_err(|e| ProverError::Boundless(format!("failed to submit request: {e}")))?;

        info!(request_id = %request_id, "proof request submitted, waiting for fulfillment");

        // Compute the expiry from timeout: pick the sooner of expires_at and
        // now + timeout.
        let timeout_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_add(self.timeout.as_secs());
        let effective_expiry = expires_at.min(timeout_at);

        let fulfillment = client
            .wait_for_request_fulfillment(request_id, self.poll_interval, effective_expiry)
            .await
            .map_err(|e| {
                warn!(error = %e, request_id = %request_id, "proof fulfillment failed");
                ProverError::Boundless(format!("fulfillment failed: {e}"))
            })?;

        let data = fulfillment
            .data()
            .map_err(|e| ProverError::Boundless(format!("failed to decode fulfillment: {e}")))?;
        let journal = data
            .journal()
            .ok_or_else(|| ProverError::Boundless("fulfillment missing journal".into()))?;

        let output = Bytes::copy_from_slice(journal);
        let proof_bytes = Bytes::copy_from_slice(&fulfillment.seal);

        info!(
            journal_len = output.len(),
            seal_len = proof_bytes.len(),
            "proof fulfilled successfully"
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
    const TEST_PROGRAM_URL: &str = "https://example.com/guest.bin";
    /// Well-known Hardhat/Anvil account #0 private key (not a real secret).
    const TEST_PRIVATE_KEY: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const TEST_IMAGE_ID: [u32; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    const TEST_MAX_PRICE_WEI: u64 = 1_000_000_000;
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
            max_price: TEST_MAX_PRICE_WEI,
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
        assert_eq!(prover.max_price, TEST_MAX_PRICE_WEI);
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
        assert_eq!(cloned.max_price, prover.max_price);
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
