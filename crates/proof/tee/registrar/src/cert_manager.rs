//! Certificate revocation management for AWS Nitro certificate chains.
//!
//! Parses Nitro attestations, checks the onchain durable revocation sentinel,
//! fetches AWS Nitro CRLs, and submits `revokeCert` transactions for
//! certificates that are newly observed on a CRL.

use std::sync::Arc;

use base_proof_tee_nitro_verifier::AttestationReport;
use base_tx_manager::TxManager;
use tracing::{debug, warn};

use crate::{
    CertRevoker, CrlConfig, NitroVerifierClient, ProverInstance, RegistrarError, RegistrarMetrics,
    Result, crl,
};

/// Manages Nitro certificate revocation checks and revocation transaction submission.
#[derive(Debug)]
pub struct CertManager<T> {
    enabled: bool,
    http_client: reqwest::Client,
    nitro_verifier: Arc<dyn NitroVerifierClient>,
    tx_manager: T,
}

impl<T> CertManager<T>
where
    T: TxManager,
{
    /// Creates a certificate manager from CRL configuration, verifier client,
    /// and transaction manager.
    ///
    /// # Errors
    ///
    /// Returns [`RegistrarError::Config`] if the CRL HTTP client cannot be built.
    pub fn new(
        config: &CrlConfig,
        nitro_verifier: Arc<dyn NitroVerifierClient>,
        tx_manager: T,
    ) -> Result<Self> {
        let http_client = crl::build_crl_http_client(config.fetch_timeout).map_err(|e| {
            RegistrarError::Config(format!(
                "failed to build CRL HTTP client (Layer 2 / AWS CRL fetch): {e}"
            ))
        })?;
        Ok(Self { enabled: config.enabled, http_client, nitro_verifier, tx_manager })
    }

    /// Checks an attestation's intermediate certificates and submits revocations.
    ///
    /// Returns `Ok(true)` if any intermediate is revoked by either the
    /// onchain sentinel or the AWS CRL layer, `Ok(false)` if every checked
    /// intermediate is clean.
    ///
    /// The durable onchain pre-check preserves previously-revoked
    /// intermediates even if AWS later prunes its CRL. AWS CRLs are then
    /// checked for each intermediate, and every CRL hit is checked onchain
    /// before a `revokeCert` transaction is submitted.
    pub async fn check_and_revoke_crls(
        &self,
        attestation_bytes: &[u8],
        instance: &ProverInstance,
    ) -> Result<bool> {
        if !self.enabled {
            return Ok(false);
        }

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
        match self.has_onchain_revoked_intermediate(&cert_infos, &instance.instance_id).await {
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
        self.submit_revocations_for_revoked_certs(&revoked_certs, instance).await;

        Ok(true)
    }

    /// Checks whether any intermediate certificate has already been revoked onchain.
    ///
    /// Root and leaf certificates are skipped because only intermediate
    /// accumulated path digests participate in the durable `revokedCerts`
    /// sentinel used by registrar CRL handling.
    ///
    /// # Errors
    ///
    /// Returns the underlying RPC error verbatim; the caller decides whether
    /// to fail-open or propagate.
    pub async fn has_onchain_revoked_intermediate(
        &self,
        cert_infos: &[crl::CertCrlInfo],
        instance_id: &str,
    ) -> Result<bool> {
        for info in crl::CertCrlInfo::intermediates(cert_infos) {
            if self.nitro_verifier.is_revoked(info.path_digest).await? {
                let cert = info.intermediate_label();
                warn!(
                    cert = %cert,
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

    /// Checks each CRL-hit against the onchain sentinel and submits needed revocations.
    pub async fn submit_revocations_for_revoked_certs(
        &self,
        revoked_certs: &[crl::RevokedCertInfo],
        instance: &ProverInstance,
    ) {
        let verifier_address = self.nitro_verifier.address();
        let cert_revoker = CertRevoker::new(verifier_address, &self.tx_manager);

        for revoked in revoked_certs {
            let cert = revoked.intermediate_label();
            match self.nitro_verifier.is_revoked(revoked.path_digest).await {
                Ok(true) => {
                    warn!(
                        cert = %cert,
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
                        cert = %cert,
                        path_digest = %revoked.path_digest,
                        instance = %instance.instance_id,
                        "onchain revocation check failed for CRL hit; submitting revokeCert"
                    );
                    RegistrarMetrics::onchain_revocation_check_errors().increment(1);
                }
            }

            warn!(
                cert = %cert,
                path_digest = %revoked.path_digest,
                instance = %instance.instance_id,
                "submitting revokeCert transaction"
            );
            cert_revoker.revoke_cert(revoked.path_digest).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicU32, Ordering},
        },
        time::Duration,
    };

    use alloy_primitives::{Address, B256, Bytes, FixedBytes};
    use alloy_sol_types::SolCall;
    use async_trait::async_trait;
    use base_proof_contracts::INitroEnclaveVerifier;
    use base_tx_manager::{TxCandidate, TxManagerError};
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
        fail_next: AtomicBool,
        call_count: AtomicU32,
    }

    impl MockNitroVerifier {
        fn revoking(hashes: impl IntoIterator<Item = FixedBytes<32>>) -> Self {
            Self {
                revoked: hashes.into_iter().collect(),
                fail_next: AtomicBool::new(false),
                call_count: AtomicU32::new(0),
            }
        }

        fn failing() -> Self {
            Self {
                revoked: HashSet::new(),
                fail_next: AtomicBool::new(true),
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
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err(RegistrarError::NitroVerifierCall {
                    context: "revokedCerts(0xdeadbeef)".into(),
                    source: "boom".into(),
                });
            }
            Ok(self.revoked.contains(&cert_hash))
        }
    }

    #[derive(Debug, Default, Clone)]
    struct MockTxManager {
        sent_candidates: Arc<Mutex<Vec<TxCandidate>>>,
    }

    impl MockTxManager {
        fn take_candidates(&self) -> Vec<TxCandidate> {
            std::mem::take(&mut *self.sent_candidates.lock().unwrap())
        }
    }

    impl TxManager for MockTxManager {
        async fn send(&self, candidate: TxCandidate) -> base_tx_manager::SendResponse {
            self.sent_candidates.lock().unwrap().push(candidate);
            Err(TxManagerError::ChannelClosed)
        }

        async fn send_async(&self, _candidate: TxCandidate) -> base_tx_manager::SendHandle {
            unreachable!("submit_revocations_for_revoked_certs only uses send")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
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

    fn test_cert_manager(verifier: Arc<MockNitroVerifier>) -> CertManager<MockTxManager> {
        CertManager {
            enabled: true,
            http_client: reqwest::Client::new(),
            nitro_verifier: verifier,
            tx_manager: MockTxManager::default(),
        }
    }

    fn test_instance() -> ProverInstance {
        ProverInstance {
            instance_id: ONCHAIN_TEST_INSTANCE_ID.to_string(),
            endpoint: "http://127.0.0.1:8000/".parse().unwrap(),
            health_status: crate::InstanceHealthStatus::Healthy,
            launch_time: None,
        }
    }

    fn crl_config() -> CrlConfig {
        CrlConfig {
            enabled: true,
            nitro_verifier_address: None,
            fetch_timeout: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn check_and_revoke_crls_noops_when_disabled() {
        let mut config = crl_config();
        config.enabled = false;
        let verifier = Arc::new(MockNitroVerifier::default());
        let cert_manager = CertManager::new(
            &config,
            Arc::<MockNitroVerifier>::clone(&verifier),
            MockTxManager::default(),
        )
        .expect("disabled cert manager still builds");

        let result =
            cert_manager.check_and_revoke_crls(b"not-an-attestation", &test_instance()).await;

        assert!(!result.expect("disabled CRL checks must no-op"));
        assert_eq!(
            verifier.call_count.load(Ordering::SeqCst),
            0,
            "disabled CRL checks must not query the verifier",
        );
    }

    fn revoked_cert(path_digest: B256) -> crl::RevokedCertInfo {
        crl::RevokedCertInfo { index: INTER1_INDEX, path_digest }
    }

    fn revoke_cert_calldata(path_digest: B256) -> Bytes {
        Bytes::from(INitroEnclaveVerifier::revokeCertCall { certHash: path_digest }.abi_encode())
    }

    async fn run_pre_check(verifier: MockNitroVerifier) -> (Result<bool>, u32) {
        let verifier = Arc::new(verifier);
        let cert_manager = test_cert_manager(Arc::clone(&verifier));
        let cert_infos = full_chain_cert_infos();
        let result = cert_manager
            .has_onchain_revoked_intermediate(&cert_infos, ONCHAIN_TEST_INSTANCE_ID)
            .await;
        (result, verifier.call_count.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn onchain_revocation_check_returns_false_when_no_intermediates_revoked() {
        let verifier = MockNitroVerifier::default();
        let (result, calls) = run_pre_check(verifier).await;

        assert!(
            !result.expect("clean chain must succeed"),
            "no intermediates flagged as revoked; registration must proceed"
        );
        assert_eq!(calls, 2, "every intermediate must be queried when none are revoked");
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
        let (result, calls) = run_pre_check(verifier).await;

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
    async fn onchain_revocation_check_propagates_rpc_errors() {
        let verifier = MockNitroVerifier::failing();
        let (result, _calls) = run_pre_check(verifier).await;

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
        let verifier = Arc::new(MockNitroVerifier::default());
        let cert_manager = test_cert_manager(Arc::clone(&verifier));

        let result = cert_manager
            .has_onchain_revoked_intermediate(&cert_infos, ONCHAIN_TEST_INSTANCE_ID)
            .await;

        assert!(!result.expect("query must succeed"), "clean chain not revoked");
        assert_eq!(
            verifier.call_count.load(Ordering::SeqCst),
            expected_calls,
            "only intermediates (root and leaf skipped) should produce RPC calls",
        );
    }

    #[tokio::test]
    async fn submit_revocations_skips_revoke_cert_when_already_revoked() {
        let path_digest = path_digest_for(INTER1_INDEX);
        let verifier = Arc::new(MockNitroVerifier::revoking([path_digest]));
        let cert_manager = test_cert_manager(Arc::clone(&verifier));
        let instance = test_instance();

        cert_manager
            .submit_revocations_for_revoked_certs(&[revoked_cert(path_digest)], &instance)
            .await;

        assert_eq!(
            verifier.call_count.load(Ordering::SeqCst),
            1,
            "CRL-hit cert should be checked onchain before deciding whether to submit revokeCert",
        );
        let candidates = cert_manager.tx_manager.take_candidates();
        assert!(candidates.is_empty(), "already-revoked certs should not submit revokeCert");
    }

    #[tokio::test]
    async fn submit_revocations_submits_revoke_cert_when_not_revoked() {
        let path_digest = path_digest_for(INTER1_INDEX);
        let verifier = Arc::new(MockNitroVerifier::default());
        let cert_manager = test_cert_manager(Arc::clone(&verifier));
        let instance = test_instance();

        cert_manager
            .submit_revocations_for_revoked_certs(&[revoked_cert(path_digest)], &instance)
            .await;

        assert_eq!(verifier.call_count.load(Ordering::SeqCst), 1);
        let candidates = cert_manager.tx_manager.take_candidates();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].to, Some(TEST_VERIFIER_ADDRESS));
        assert_eq!(candidates[0].tx_data, revoke_cert_calldata(path_digest));
    }

    #[tokio::test]
    async fn submit_revocations_submits_revoke_cert_when_onchain_check_errors() {
        let path_digest = path_digest_for(INTER1_INDEX);
        let verifier = Arc::new(MockNitroVerifier::failing());
        let cert_manager = test_cert_manager(Arc::clone(&verifier));
        let instance = test_instance();

        cert_manager
            .submit_revocations_for_revoked_certs(&[revoked_cert(path_digest)], &instance)
            .await;

        assert_eq!(verifier.call_count.load(Ordering::SeqCst), 1);
        let candidates = cert_manager.tx_manager.take_candidates();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].to, Some(TEST_VERIFIER_ADDRESS));
        assert_eq!(candidates[0].tx_data, revoke_cert_calldata(path_digest));
    }
}
