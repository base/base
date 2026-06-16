//! Certificate revocation management for AWS Nitro certificate chains.
//!
//! Parses Nitro attestations, checks the onchain durable revocation sentinel,
//! fetches AWS Nitro CRLs, and submits `revokeCert` transactions for
//! certificates that are newly observed on a CRL.

use alloy_primitives::{Bytes, FixedBytes};
use alloy_sol_types::SolCall;
use base_proof_contracts::INitroEnclaveVerifier;
use base_proof_tee_nitro_verifier::AttestationReport;
use base_tx_manager::{TxCandidate, TxManager};
use tracing::{debug, info, warn};

use crate::{
    CrlConfig, NitroVerifierClient, ProverInstance, RegistrarError, RegistrarMetrics, Result, crl,
};

/// Manages Nitro certificate revocation checks and revocation transaction submission.
#[derive(Debug)]
pub struct CertManager<T> {
    enabled: bool,
    http_client: reqwest::Client,
    nitro_verifier: Box<dyn NitroVerifierClient>,
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
        nitro_verifier: Box<dyn NitroVerifierClient>,
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

        for revoked in &revoked_certs {
            warn!(
                cert_index = revoked.index,
                path_digest = %revoked.path_digest,
                instance = %instance.instance_id,
                "submitting revokeCert transaction"
            );

            self.submit_revoke_cert(revoked.path_digest).await;
        }

        Ok(true)
    }

    /// Submits a `NitroEnclaveVerifier.revokeCert` transaction.
    pub async fn submit_revoke_cert(&self, path_digest: FixedBytes<32>) {
        let verifier_address = self.nitro_verifier.address();
        let calldata = Bytes::from(
            INitroEnclaveVerifier::revokeCertCall { certHash: path_digest }.abi_encode(),
        );
        let candidate =
            TxCandidate { tx_data: calldata, to: Some(verifier_address), ..Default::default() };

        info!(
            path_digest = %path_digest,
            verifier = %verifier_address,
            calldata_len = candidate.tx_data.len(),
            "sending revokeCert transaction"
        );

        match self.tx_manager.send(candidate).await {
            Ok(receipt) => {
                if !receipt.inner.status() {
                    warn!(
                        path_digest = %path_digest,
                        tx_hash = %receipt.transaction_hash,
                        "revokeCert transaction reverted (cert may already be revoked)"
                    );
                    RegistrarMetrics::revoke_cert_reverted_total().increment(1);
                } else {
                    info!(
                        path_digest = %path_digest,
                        tx_hash = %receipt.transaction_hash,
                        "certificate revoked successfully"
                    );
                    RegistrarMetrics::revoke_cert_success_total().increment(1);
                }
            }
            Err(e) => {
                warn!(
                    error = %e,
                    path_digest = %path_digest,
                    "failed to submit revokeCert transaction"
                );
                RegistrarMetrics::revoke_cert_tx_failures().increment(1);
            }
        }
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
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    };

    use alloy_primitives::{Address, B256, Bytes};
    use alloy_sol_types::SolCall;
    use async_trait::async_trait;
    use base_proof_contracts::INitroEnclaveVerifier;
    use base_tx_manager::{SendHandle, TxCandidate, TxManager, TxManagerError};

    use super::*;
    use crate::test_utils::{NoopNitroVerifier, NoopTxManager, healthy_prover_instance};

    /// Mock [`NitroVerifierClient`] for unit-testing the onchain pre-check.
    #[derive(Debug, Default)]
    struct MockNitroVerifier {
        revoked: Option<B256>,
        fail: bool,
        call_count: Arc<AtomicU32>,
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
            path_digest: B256::repeat_byte(index as u8),
        }
    }

    #[tokio::test]
    async fn check_and_revoke_crls_noops_when_disabled() {
        let config = CrlConfig {
            enabled: false,
            nitro_verifier_address: None,
            fetch_timeout: std::time::Duration::from_secs(1),
        };
        let cert_manager = CertManager::new(&config, Box::new(NoopNitroVerifier), NoopTxManager)
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
        let call_count = Arc::clone(&verifier.call_count);
        let cert_manager = CertManager {
            enabled: true,
            http_client: reqwest::Client::new(),
            nitro_verifier: Box::new(verifier),
            tx_manager: NoopTxManager,
        };
        let cert_infos = (0..=3).map(cert_info).collect::<Vec<_>>();
        let result = cert_manager
            .has_onchain_revoked_intermediate(&cert_infos, "i-onchain-revocation-test")
            .await;
        (result, call_count.load(Ordering::Relaxed))
    }

    #[derive(Debug, Default)]
    struct CapturingTxManager {
        sent_candidate: Mutex<Option<TxCandidate>>,
    }

    impl CapturingTxManager {
        fn take_candidate(&self) -> TxCandidate {
            self.sent_candidate.lock().unwrap().take().expect("candidate was sent")
        }
    }

    impl TxManager for CapturingTxManager {
        async fn send(&self, candidate: TxCandidate) -> base_tx_manager::SendResponse {
            *self.sent_candidate.lock().unwrap() = Some(candidate);
            Err(TxManagerError::NonceTooLow)
        }

        async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
            unreachable!("cert manager tests use synchronous send")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    #[tokio::test]
    async fn submit_revoke_cert_sends_revoke_cert_candidate() {
        let cert_manager = CertManager {
            enabled: true,
            http_client: reqwest::Client::new(),
            nitro_verifier: Box::new(NoopNitroVerifier),
            tx_manager: CapturingTxManager::default(),
        };
        let path_digest = B256::repeat_byte(0x22);

        cert_manager.submit_revoke_cert(path_digest).await;

        let candidate = cert_manager.tx_manager.take_candidate();
        assert_eq!(candidate.to, Some(Address::ZERO));
        assert_eq!(
            candidate.tx_data,
            Bytes::from(
                INitroEnclaveVerifier::revokeCertCall { certHash: path_digest }.abi_encode()
            )
        );
    }

    #[tokio::test]
    async fn onchain_revocation_check_reports_intermediate_status() {
        for (revoked_index, expected_result, expected_calls) in
            [(None, false, 2), (Some(1), true, 1), (Some(2), true, 2)]
        {
            let verifier = MockNitroVerifier {
                revoked: revoked_index.map(|index| B256::repeat_byte(index as u8)),
                ..MockNitroVerifier::default()
            };
            let (result, calls) = run_pre_check(verifier).await;

            assert_eq!(result.expect("revocation query succeeds"), expected_result);
            assert_eq!(calls, expected_calls);
        }
    }

    #[tokio::test]
    async fn onchain_revocation_check_propagates_rpc_errors() {
        let verifier = MockNitroVerifier { fail: true, ..MockNitroVerifier::default() };
        let (result, _calls) = run_pre_check(verifier).await;

        let err = result.expect_err("RPC errors must surface to the caller");
        assert!(
            matches!(err, RegistrarError::NitroVerifierCall { .. }),
            "expected NitroVerifierCall, got: {err:?}"
        );
    }
}
