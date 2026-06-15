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
    pub async fn check_and_revoke_crls(
        &self,
        attestation_bytes: &[u8],
        instance: &ProverInstance,
    ) -> Result<bool> {
        if !self.enabled {
            return Ok(false);
        }

        let report = AttestationReport::parse(attestation_bytes).map_err(|e| {
            RegistrarError::ProverClient {
                instance: instance.endpoint.to_string(),
                source: format!("failed to parse attestation for CRL check: {e}").into(),
            }
        })?;
        let cert_infos = crl::CertCrlInfo::from_chain(&report.cert_chain_der())?;

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
        let verifier_address = self.nitro_verifier.address();
        let cert_revoker = CertRevoker::new(verifier_address, &self.tx_manager);

        for revoked in &revoked_certs {
            warn!(
                cert_index = revoked.index,
                path_digest = %revoked.path_digest,
                instance = %instance.instance_id,
                "submitting revokeCert transaction"
            );
            cert_revoker.revoke_cert(revoked.path_digest).await;
        }

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
                warn!(
                    cert_index = info.index,
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
    use std::sync::atomic::{AtomicU32, Ordering};

    use alloy_primitives::{Address, B256};
    use async_trait::async_trait;

    use super::*;
    use crate::test_utils::{NoopNitroVerifier, NoopTxManager, healthy_prover_instance};

    const ONCHAIN_TEST_INSTANCE_ID: &str = "i-onchain-revocation-test";
    const INTER1_INDEX: usize = 1;
    const INTER2_INDEX: usize = 2;

    /// Mock [`NitroVerifierClient`] for unit-testing the onchain pre-check.
    #[derive(Debug, Default)]
    struct MockNitroVerifier {
        revoked: Option<B256>,
        fail: bool,
        call_count: AtomicU32,
    }

    impl MockNitroVerifier {
        fn revoking(hash: B256) -> Self {
            Self { revoked: Some(hash), ..Self::default() }
        }

        fn failing() -> Self {
            Self { fail: true, ..Self::default() }
        }

        fn call_count(&self) -> u32 {
            self.call_count.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl crate::NitroVerifierClient for MockNitroVerifier {
        fn address(&self) -> Address {
            Address::ZERO
        }

        async fn is_revoked(&self, cert_hash: B256) -> Result<bool> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                return Err(RegistrarError::NitroVerifierCall {
                    context: "revokedCerts(0xdeadbeef)".into(),
                    source: "boom".into(),
                });
            }
            Ok(self.revoked == Some(cert_hash))
        }
    }

    fn cert_info(index: usize) -> crl::CertCrlInfo {
        crl::CertCrlInfo {
            index,
            serial_number: Vec::new(),
            crl_url: None,
            path_digest: path_digest_for(index),
        }
    }

    fn path_digest_for(index: usize) -> B256 {
        B256::repeat_byte(index as u8)
    }

    fn test_cert_manager(verifier: Arc<MockNitroVerifier>) -> CertManager<NoopTxManager> {
        CertManager {
            enabled: true,
            http_client: reqwest::Client::new(),
            nitro_verifier: verifier,
            tx_manager: NoopTxManager,
        }
    }

    #[tokio::test]
    async fn check_and_revoke_crls_noops_when_disabled() {
        let config = CrlConfig {
            enabled: false,
            nitro_verifier_address: None,
            fetch_timeout: std::time::Duration::from_secs(1),
        };
        let cert_manager = CertManager::new(&config, Arc::new(NoopNitroVerifier), NoopTxManager)
            .expect("disabled cert manager still builds");

        let result = cert_manager
            .check_and_revoke_crls(
                b"not-an-attestation",
                &healthy_prover_instance("127.0.0.1:8000"),
            )
            .await;

        assert!(!result.expect("disabled CRL checks must no-op"));
    }

    async fn run_pre_check(verifier: MockNitroVerifier) -> (Result<bool>, u32) {
        let verifier = Arc::new(verifier);
        let cert_manager = test_cert_manager(Arc::clone(&verifier));
        let cert_infos = (0..=3).map(cert_info).collect::<Vec<_>>();
        let result = cert_manager
            .has_onchain_revoked_intermediate(&cert_infos, ONCHAIN_TEST_INSTANCE_ID)
            .await;
        (result, verifier.call_count())
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

    #[tokio::test]
    async fn onchain_revocation_check_blocks_when_any_intermediate_revoked() {
        for (revoked_index, expected_calls_at_short_circuit) in
            [(INTER1_INDEX, 1), (INTER2_INDEX, 2)]
        {
            let verifier = MockNitroVerifier::revoking(path_digest_for(revoked_index));
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
}
