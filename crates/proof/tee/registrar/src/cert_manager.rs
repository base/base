//! Certificate revocation management for AWS Nitro certificate chains.
//!
//! Parses Nitro attestations, checks the onchain durable revocation sentinel,
//! fetches AWS Nitro CRLs, and submits `revokeCert` transactions for
//! certificates that are newly observed on a CRL.

use std::time::Duration;

use alloy_primitives::Bytes;
use alloy_sol_types::SolCall;
use base_proof_contracts::{
    INitroEnclaveVerifier, NitroEnclaveVerifierClient, caller_not_owner_or_revoker_selector,
};
use base_proof_tee_nitro_verifier::AttestationReport;
use base_tx_manager::{TxCandidate, TxManager, TxManagerError};
use tracing::{info, warn};

use crate::{ProverInstance, RegistrarError, RegistrarMetrics, Result, crl};

/// Manages Nitro certificate revocation checks and revocation transaction submission.
#[derive(Debug)]
pub struct CertManager<T> {
    http_client: reqwest::Client,
    nitro_verifier: Box<dyn NitroEnclaveVerifierClient>,
    tx_manager: T,
}

impl<T> CertManager<T>
where
    T: TxManager,
{
    /// Decodes known `NitroEnclaveVerifier` custom-error names from a tx-manager error.
    fn revoke_cert_revert_name(err: &TxManagerError) -> Option<&'static str> {
        Self::revoke_cert_revert_selector(err)
            .and_then(INitroEnclaveVerifier::INitroEnclaveVerifierErrors::name_by_selector)
    }

    /// Decodes the raw revert selector from a tx-manager execution revert.
    fn revoke_cert_revert_selector(err: &TxManagerError) -> Option<[u8; 4]> {
        let TxManagerError::ExecutionReverted { data, .. } = err else {
            return None;
        };

        data.as_ref().and_then(|d| d.get(..4)).and_then(|selector| selector.try_into().ok())
    }

    /// Returns whether a tx-manager error is `CallerNotOwnerOrRevoker()`.
    fn is_revoke_cert_authorization_error(err: &TxManagerError) -> bool {
        Self::revoke_cert_revert_selector(err)
            .is_some_and(|selector| selector == caller_not_owner_or_revoker_selector())
    }

    /// Creates a certificate manager from CRL fetch timeout, verifier client, and transaction manager.
    ///
    /// # Errors
    ///
    /// Returns an error if the CRL HTTP client cannot be built.
    pub fn new(
        fetch_timeout: Duration,
        nitro_verifier: Box<dyn NitroEnclaveVerifierClient>,
        tx_manager: T,
    ) -> Result<Self> {
        let http_client = reqwest::Client::builder()
            .timeout(fetch_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| RegistrarError::Config(format!("failed to build HTTP client: {e}")))?;
        Ok(Self { http_client, nitro_verifier, tx_manager })
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
        let report = AttestationReport::parse(attestation_bytes).map_err(|e| {
            RegistrarError::ProverClient {
                instance: instance.endpoint.to_string(),
                source: format!("failed to parse attestation for CRL check: {e}").into(),
            }
        })?;
        let cert_infos = crl::CertCrlInfo::from_chain(&report.cert_chain_der())?;

        RegistrarMetrics::onchain_revocation_checks_total().increment(1);
        for info in &cert_infos {
            match self.nitro_verifier.is_revoked(info.path_digest).await {
                Ok(true) => {
                    warn!(
                        cert_index = info.index,
                        path_digest = %info.path_digest,
                        instance = %instance.instance_id,
                        "intermediate is revoked onchain (durable sentinel set), skipping registration"
                    );
                    RegistrarMetrics::onchain_revocations_detected().increment(1);
                    return Ok(true);
                }
                Ok(false) => {}
                Err(e) => {
                    warn!(
                        error = %e,
                        instance = %instance.instance_id,
                        "onchain revocation pre-check failed; falling through to AWS CRL layer"
                    );
                    RegistrarMetrics::onchain_revocation_check_errors().increment(1);
                    break;
                }
            }
        }

        RegistrarMetrics::crl_checks_total().increment(1);
        let revoked_certs = crl::check_chain_against_crls(&cert_infos, &self.http_client).await;

        if revoked_certs.is_empty() {
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
            match self
                .tx_manager
                .send(TxCandidate {
                    tx_data: Bytes::from(
                        INitroEnclaveVerifier::revokeCertCall { certHash: revoked.path_digest }
                            .abi_encode(),
                    ),
                    to: Some(self.nitro_verifier.address()),
                    ..Default::default()
                })
                .await
            {
                Ok(receipt) if !receipt.inner.status() => {
                    warn!(
                        path_digest = %revoked.path_digest,
                        tx_hash = %receipt.transaction_hash,
                        "revokeCert transaction reverted after inclusion; receipt has no revert data"
                    );
                    RegistrarMetrics::revoke_cert_reverted_total().increment(1);
                }
                Ok(receipt) => {
                    info!(
                        path_digest = %revoked.path_digest,
                        tx_hash = %receipt.transaction_hash,
                        "certificate revoked successfully"
                    );
                    RegistrarMetrics::revoke_cert_success_total().increment(1);
                }
                Err(e) => {
                    let nitro_error = Self::revoke_cert_revert_name(&e);
                    if Self::is_revoke_cert_authorization_error(&e) {
                        warn!(
                            error = %e,
                            nitro_error = ?nitro_error,
                            path_digest = %revoked.path_digest,
                            "revokeCert sender is not authorized; configure registrar signer as Nitro owner or revoker"
                        );
                    } else {
                        warn!(
                            error = %e,
                            nitro_error = ?nitro_error,
                            path_digest = %revoked.path_digest,
                            "failed to submit revokeCert transaction"
                        );
                    }
                    RegistrarMetrics::revoke_cert_tx_failures().increment(1);
                }
            }
        }

        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use alloy_primitives::{Address, B256, Bytes};
    use async_trait::async_trait;
    use base_proof_contracts::ContractError;
    use base_tx_manager::TxManagerError;

    use super::*;
    use crate::test_utils::{EP1, NoopTxManager, healthy_prover_instance};

    #[derive(Debug)]
    struct TestNitroVerifier {
        revoked: Option<B256>,
        fails: bool,
        calls: Arc<Mutex<Vec<B256>>>,
    }

    #[async_trait]
    impl NitroEnclaveVerifierClient for TestNitroVerifier {
        fn address(&self) -> Address {
            Address::repeat_byte(0x42)
        }

        async fn is_revoked(&self, cert_hash: B256) -> std::result::Result<bool, ContractError> {
            self.calls.lock().unwrap().push(cert_hash);
            if self.fails {
                return Err(ContractError::validation("rpc unavailable"));
            }
            Ok(self.revoked == Some(cert_hash))
        }
    }

    fn manager(nitro_verifier: Box<dyn NitroEnclaveVerifierClient>) -> CertManager<NoopTxManager> {
        CertManager::new(Duration::from_millis(10), nitro_verifier, NoopTxManager).unwrap()
    }

    fn attestation() -> Vec<u8> {
        hex::decode(include_str!("testdata/minimal_attestation.hex").trim()).unwrap()
    }

    fn intermediate_digests(attestation: &[u8]) -> Vec<B256> {
        let report = AttestationReport::parse(attestation).unwrap();
        let cert_infos = crl::CertCrlInfo::from_chain(&report.cert_chain_der()).unwrap();
        cert_infos.iter().map(|info| info.path_digest).collect()
    }

    #[tokio::test]
    async fn parse_failure_includes_instance_endpoint() {
        let manager = manager(Box::new(TestNitroVerifier {
            revoked: None,
            fails: false,
            calls: Arc::new(Mutex::new(Vec::new())),
        }));

        let err = manager
            .check_and_revoke_crls(b"not a cose attestation", &healthy_prover_instance(EP1))
            .await
            .unwrap_err();

        let RegistrarError::ProverClient { instance, source } = err else {
            panic!("expected ProverClient parse error, got {err:?}");
        };
        assert_eq!(instance, "http://10.0.0.1:8000/");
        assert!(source.to_string().contains("failed to parse attestation for CRL check"));
    }

    #[tokio::test]
    async fn onchain_revocation_short_circuits_before_crl_and_tx() {
        let attestation = attestation();
        let digests = intermediate_digests(&attestation);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let manager = manager(Box::new(TestNitroVerifier {
            revoked: Some(digests[0]),
            fails: false,
            calls: Arc::clone(&calls),
        }));

        let revoked = manager
            .check_and_revoke_crls(&attestation, &healthy_prover_instance(EP1))
            .await
            .unwrap();

        assert!(revoked);
        assert_eq!(*calls.lock().unwrap(), vec![digests[0]]);
    }

    #[tokio::test]
    async fn onchain_error_falls_through_to_crl_layer_fail_open() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let manager = manager(Box::new(TestNitroVerifier {
            revoked: None,
            fails: true,
            calls: Arc::clone(&calls),
        }));
        let attestation = attestation();

        let revoked = manager
            .check_and_revoke_crls(&attestation, &healthy_prover_instance(EP1))
            .await
            .unwrap();

        assert!(!revoked);
    }

    #[test]
    fn revoke_cert_revert_name_decodes_authorization_error() {
        let err = TxManagerError::ExecutionReverted {
            reason: None,
            data: Some(Bytes::from(caller_not_owner_or_revoker_selector().to_vec())),
        };

        assert_eq!(
            CertManager::<NoopTxManager>::revoke_cert_revert_name(&err),
            Some("CallerNotOwnerOrRevoker"),
        );
        assert!(CertManager::<NoopTxManager>::is_revoke_cert_authorization_error(&err));
    }

    #[test]
    fn revoke_cert_revert_name_ignores_unrelated_tx_errors() {
        assert_eq!(
            CertManager::<NoopTxManager>::revoke_cert_revert_name(&TxManagerError::NonceTooLow),
            None,
        );
        assert!(!CertManager::<NoopTxManager>::is_revoke_cert_authorization_error(
            &TxManagerError::NonceTooLow,
        ));
    }
}
