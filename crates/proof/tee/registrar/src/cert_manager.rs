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
        let onchain_revoked: Result<bool> = async {
            for info in crl::CertCrlInfo::intermediates(&cert_infos) {
                if self.nitro_verifier.is_revoked(info.path_digest).await? {
                    warn!(
                        cert_index = info.index,
                        path_digest = %info.path_digest,
                        instance = %instance.instance_id,
                        "intermediate is revoked onchain (durable sentinel set), skipping registration"
                    );
                    RegistrarMetrics::onchain_revocations_detected().increment(1);
                    return Ok(true);
                }
            }

            debug!(instance = %instance.instance_id, "onchain revocation pre-check passed");
            Ok(false)
        }
        .await;
        match onchain_revoked {
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
            Ok(receipt) if !receipt.inner.status() => {
                warn!(
                    path_digest = %path_digest,
                    tx_hash = %receipt.transaction_hash,
                    "revokeCert transaction reverted (cert may already be revoked)"
                );
                RegistrarMetrics::revoke_cert_reverted_total().increment(1);
            }
            Ok(receipt) => {
                info!(
                    path_digest = %path_digest,
                    tx_hash = %receipt.transaction_hash,
                    "certificate revoked successfully"
                );
                RegistrarMetrics::revoke_cert_success_total().increment(1);
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
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use alloy_primitives::{Address, B256, Bytes};
    use alloy_sol_types::SolCall;
    use base_proof_contracts::INitroEnclaveVerifier;
    use base_tx_manager::{SendHandle, TxCandidate, TxManager, TxManagerError};

    use super::*;
    use crate::test_utils::{NoopNitroVerifier, NoopTxManager, healthy_prover_instance};

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
}
