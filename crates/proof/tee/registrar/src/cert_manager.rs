//! Certificate revocation management for AWS Nitro certificate chains.
//!
//! Parses Nitro attestations, checks the onchain durable revocation sentinel,
//! fetches AWS Nitro CRLs, and submits `revokeCert` transactions for
//! certificates that are newly observed on a CRL.

use std::{fmt, sync::Arc};

use base_proof_tee_nitro_verifier::AttestationReport;
use base_tx_manager::TxManager;
use tracing::{debug, warn};

use crate::{
    CertRevoker, CrlConfig, NitroVerifierClient, ProverInstance, RegistrarError, RegistrarMetrics,
    Result, crl,
};

/// Manages Nitro certificate revocation checks and revocation transaction submission.
pub struct CertManager {
    http_client: reqwest::Client,
    nitro_verifier: Arc<dyn NitroVerifierClient>,
}

impl fmt::Debug for CertManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertManager").finish_non_exhaustive()
    }
}

impl CertManager {
    /// Creates a certificate manager from CRL configuration and a verifier client.
    ///
    /// # Errors
    ///
    /// Returns [`RegistrarError::Config`] if the CRL HTTP client cannot be built.
    pub fn new(config: &CrlConfig, nitro_verifier: Arc<dyn NitroVerifierClient>) -> Result<Self> {
        let http_client = crl::build_crl_http_client(config.fetch_timeout).map_err(|e| {
            RegistrarError::Config(format!(
                "failed to build CRL HTTP client (Layer 2 / AWS CRL fetch): {e}"
            ))
        })?;
        Ok(Self::from_parts(http_client, nitro_verifier))
    }

    /// Creates a certificate manager from pre-built dependencies.
    pub fn from_parts(
        http_client: reqwest::Client,
        nitro_verifier: Arc<dyn NitroVerifierClient>,
    ) -> Self {
        Self { http_client, nitro_verifier }
    }

    /// Checks an attestation's intermediate certificates and submits revocations.
    ///
    /// Returns `Ok(true)` if any intermediate is revoked by either the
    /// onchain sentinel or the AWS CRL layer, `Ok(false)` if every checked
    /// intermediate is clean.
    ///
    /// The durable onchain pre-check preserves previously-revoked
    /// intermediates even if AWS later prunes its CRL. AWS CRL fetches are
    /// then performed in parallel, and every CRL hit is checked onchain
    /// before a `revokeCert` transaction is submitted.
    pub async fn check_and_revoke_crls<T>(
        &self,
        attestation_bytes: &[u8],
        instance: &ProverInstance,
        tx_manager: &T,
    ) -> Result<bool>
    where
        T: TxManager,
    {
        let cert_infos = {
            let report = AttestationReport::parse(attestation_bytes).map_err(|e| {
                RegistrarError::ProverClient {
                    instance: instance.endpoint.to_string(),
                    source: format!("failed to parse attestation for CRL check: {e}").into(),
                }
            })?;
            let cert_chain_der = report.cert_chain_der();
            crl::CertCrlInfo::from_chain(&cert_chain_der)?
        };

        RegistrarMetrics::onchain_revocation_checks_total().increment(1);
        match OnchainRevocationCheck::run(
            self.nitro_verifier.as_ref(),
            &cert_infos,
            &instance.instance_id,
        )
        .await
        {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(e) => {
                warn!(
                    error = %e,
                    instance = %instance.instance_id,
                    "onchain revocation pre-check failed; falling through to AWS CRL layer"
                );
                RegistrarMetrics::onchain_revocation_check_errors().increment(1);
            }
        }

        RegistrarMetrics::crl_checks_total().increment(1);
        let revoked_certs = crl::check_chain_against_crls(&cert_infos, &self.http_client).await;

        if revoked_certs.is_empty() {
            debug!(instance = %instance.instance_id, "CRL check passed, all certs clean");
            return Ok(false);
        }

        RegistrarMetrics::crl_revocations_detected().increment(revoked_certs.len() as u64);
        self.submit_revocations_for_revoked_certs(&revoked_certs, instance, tx_manager).await;

        Ok(true)
    }

    /// Checks each CRL-hit against the onchain sentinel and submits needed revocations.
    pub async fn submit_revocations_for_revoked_certs<T>(
        &self,
        revoked_certs: &[crl::RevokedCertInfo],
        instance: &ProverInstance,
        tx_manager: &T,
    ) where
        T: TxManager,
    {
        let verifier_address = self.nitro_verifier.address();
        let cert_revoker = CertRevoker::new(verifier_address, tx_manager);

        for revoked in revoked_certs {
            match self.nitro_verifier.is_revoked(revoked.path_digest).await {
                Ok(true) => {
                    warn!(
                        cert = %revoked.label,
                        path_digest = %revoked.path_digest,
                        instance = %instance.instance_id,
                        "certificate already revoked onchain, skipping revokeCert"
                    );
                    RegistrarMetrics::onchain_revocations_detected().increment(1);
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    warn!(
                        error = %e,
                        cert = %revoked.label,
                        path_digest = %revoked.path_digest,
                        instance = %instance.instance_id,
                        "onchain revocation check failed for CRL hit; submitting revokeCert"
                    );
                    RegistrarMetrics::onchain_revocation_check_errors().increment(1);
                }
            }

            warn!(
                cert = %revoked.label,
                path_digest = %revoked.path_digest,
                instance = %instance.instance_id,
                "submitting revokeCert transaction"
            );
            cert_revoker.revoke_cert(revoked.path_digest).await;
        }
    }
}

/// Onchain durable revocation check.
///
/// Consults the `revokedCerts` mapping for every intermediate in a pre-parsed
/// Nitro cert chain. Root and leaf certificates are skipped.
#[derive(Debug)]
pub struct OnchainRevocationCheck;

impl OnchainRevocationCheck {
    /// Returns `Ok(true)` as soon as any intermediate's accumulated path
    /// digest is found revoked onchain, `Ok(false)` if every intermediate
    /// is clean.
    ///
    /// # Errors
    ///
    /// Returns the underlying RPC error verbatim; the caller decides whether
    /// to fail-open or propagate.
    pub async fn run(
        verifier: &dyn NitroVerifierClient,
        cert_infos: &[crl::CertCrlInfo],
        instance_id: &str,
    ) -> Result<bool> {
        for info in crl::CertCrlInfo::intermediates(cert_infos) {
            if verifier.is_revoked(info.path_digest).await? {
                warn!(
                    cert = %info.label,
                    path_digest = %info.path_digest,
                    instance = %instance_id,
                    "intermediate is revoked onchain (durable sentinel set), skipping registration"
                );
                RegistrarMetrics::onchain_revocations_detected().increment(1);
                return Ok(true);
            }
        }

        debug!(instance = %instance_id, "onchain revocation pre-check passed");
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{
            Mutex,
            atomic::{AtomicU32, Ordering},
        },
    };

    use alloy_primitives::{Address, FixedBytes};
    use async_trait::async_trait;
    use rstest::rstest;

    use super::*;
    use crate::test_utils::{CertFixtures, INTER1_HEX, INTER2_HEX, LEAF_HEX, ROOT_HEX};

    const ONCHAIN_TEST_INSTANCE_ID: &str = "i-onchain-revocation-test";
    const TEST_VERIFIER_ADDRESS: Address = Address::repeat_byte(0xAB);
    const ROOT_INDEX: usize = 0;
    const INTER1_INDEX: usize = 1;
    const INTER2_INDEX: usize = 2;
    const LEAF_INDEX: usize = 3;

    /// Mock [`NitroVerifierClient`] for unit-testing the onchain pre-check.
    #[derive(Debug, Default)]
    struct MockNitroVerifier {
        revoked: HashSet<FixedBytes<32>>,
        error: Mutex<Option<RegistrarError>>,
        call_count: AtomicU32,
    }

    impl MockNitroVerifier {
        fn revoking(hashes: impl IntoIterator<Item = FixedBytes<32>>) -> Self {
            Self {
                revoked: hashes.into_iter().collect(),
                error: Mutex::new(None),
                call_count: AtomicU32::new(0),
            }
        }

        fn failing(error: RegistrarError) -> Self {
            Self {
                revoked: HashSet::new(),
                error: Mutex::new(Some(error)),
                call_count: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl crate::NitroVerifierClient for MockNitroVerifier {
        fn address(&self) -> Address {
            TEST_VERIFIER_ADDRESS
        }

        async fn is_revoked(&self, cert_hash: FixedBytes<32>) -> Result<bool> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if let Some(err) = self.error.lock().unwrap().take() {
                return Err(err);
            }
            Ok(self.revoked.contains(&cert_hash))
        }
    }

    fn full_chain_der() -> Vec<Vec<u8>> {
        CertFixtures::decode_chain(&[ROOT_HEX, INTER1_HEX, INTER2_HEX, LEAF_HEX])
    }

    fn chain_subset(indices: &[usize]) -> Vec<Vec<u8>> {
        let full = full_chain_der();
        indices.iter().map(|&i| full[i].clone()).collect()
    }

    fn path_digest_for(index: usize) -> FixedBytes<32> {
        let der = full_chain_der();
        let refs: Vec<&[u8]> = der.iter().map(Vec::as_slice).collect();
        crl::CertCrlInfo::from_chain(&refs)
            .expect("static fixtures parse")
            .remove(index)
            .path_digest
    }

    fn full_chain_cert_infos() -> Vec<crl::CertCrlInfo> {
        let der = full_chain_der();
        let refs: Vec<&[u8]> = der.iter().map(Vec::as_slice).collect();
        crl::CertCrlInfo::from_chain(&refs).expect("static fixtures parse")
    }

    fn full_chain_intermediate_count() -> u32 {
        u32::try_from(full_chain_der().len().saturating_sub(2)).unwrap()
    }

    async fn run_pre_check(verifier: &MockNitroVerifier) -> (Result<bool>, u32) {
        let cert_infos = full_chain_cert_infos();
        let result =
            OnchainRevocationCheck::run(verifier, &cert_infos, ONCHAIN_TEST_INSTANCE_ID).await;
        (result, verifier.call_count.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn onchain_revocation_check_returns_false_when_no_intermediates_revoked() {
        let verifier = MockNitroVerifier::default();
        let (result, calls) = run_pre_check(&verifier).await;

        assert!(
            !result.expect("clean chain must succeed"),
            "no intermediates flagged as revoked; registration must proceed"
        );
        assert_eq!(
            calls,
            full_chain_intermediate_count(),
            "every intermediate must be queried when none are revoked"
        );
    }

    #[rstest]
    #[case::inter1_revoked(INTER1_INDEX, 1)]
    #[case::inter2_revoked(INTER2_INDEX, 2)]
    #[tokio::test]
    async fn onchain_revocation_check_blocks_when_any_intermediate_revoked(
        #[case] revoked_index: usize,
        #[case] expected_calls_at_short_circuit: u32,
    ) {
        let verifier = MockNitroVerifier::revoking([path_digest_for(revoked_index)]);
        let (result, calls) = run_pre_check(&verifier).await;

        assert!(
            result.expect("revoked-intermediate query must succeed"),
            "revoked intermediate must block registration",
        );
        assert_eq!(
            calls, expected_calls_at_short_circuit,
            "pre-check must short-circuit at the first revoked intermediate",
        );
    }

    #[tokio::test]
    async fn onchain_revocation_check_short_circuits_when_all_intermediates_revoked() {
        let verifier = MockNitroVerifier::revoking([
            path_digest_for(INTER1_INDEX),
            path_digest_for(INTER2_INDEX),
        ]);
        let (result, calls) = run_pre_check(&verifier).await;

        assert!(result.expect("query must succeed"), "any revoked intermediate must block");
        assert_eq!(calls, 1, "first intermediate triggers short-circuit");
    }

    #[tokio::test]
    async fn onchain_revocation_check_skips_root_and_leaf() {
        let verifier =
            MockNitroVerifier::revoking([path_digest_for(ROOT_INDEX), path_digest_for(LEAF_INDEX)]);
        let (result, calls) = run_pre_check(&verifier).await;

        assert!(
            !result.expect("query must succeed"),
            "root/leaf revocation flags must not block registration",
        );
        assert_eq!(
            calls,
            full_chain_intermediate_count(),
            "only intermediates are queried; root and leaf are skipped",
        );
    }

    #[tokio::test]
    async fn onchain_revocation_check_propagates_rpc_errors() {
        let verifier = MockNitroVerifier::failing(RegistrarError::NitroVerifierCall {
            context: "revokedCerts(0xdeadbeef)".into(),
            source: "boom".into(),
        });
        let (result, _calls) = run_pre_check(&verifier).await;

        let err = result.expect_err("RPC errors must surface to the caller");
        assert!(
            matches!(err, RegistrarError::NitroVerifierCall { .. }),
            "expected NitroVerifierCall, got: {err:?}"
        );
    }

    #[rstest]
    #[case::root_only(&[ROOT_INDEX], 0)]
    #[case::root_and_leaf(&[ROOT_INDEX, LEAF_INDEX], 0)]
    #[case::three_cert(&[ROOT_INDEX, INTER1_INDEX, LEAF_INDEX], 1)]
    #[tokio::test]
    async fn onchain_revocation_check_queries_intermediates_only(
        #[case] indices: &[usize],
        #[case] expected_calls: u32,
    ) {
        let owned = chain_subset(indices);
        let refs: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
        let cert_infos = crl::CertCrlInfo::from_chain(&refs).expect("static fixtures parse");
        let verifier = MockNitroVerifier::default();

        let result =
            OnchainRevocationCheck::run(&verifier, &cert_infos, ONCHAIN_TEST_INSTANCE_ID).await;

        assert!(!result.expect("query must succeed"), "clean chain not revoked");
        assert_eq!(
            verifier.call_count.load(Ordering::SeqCst),
            expected_calls,
            "only intermediates (root and leaf skipped) should produce RPC calls",
        );
    }
}
