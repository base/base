//! Certificate revocation management for AWS Nitro certificate chains.
//!
//! Parses Nitro attestations, checks the onchain durable revocation sentinel,
//! fetches AWS Nitro CRLs, and submits `revokeCert` transactions for
//! certificates that are newly observed on a CRL.

use alloy_primitives::Bytes;
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
    /// Returns an error if the CRL HTTP client cannot be built.
    pub fn new(
        config: &CrlConfig,
        nitro_verifier: Box<dyn NitroVerifierClient>,
        tx_manager: T,
    ) -> Result<Self> {
        let http_client = crl::build_crl_http_client(config.fetch_timeout)?;
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
        for info in crl::CertCrlInfo::intermediates(&cert_infos) {
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
            debug!(instance = %instance.instance_id, "CRL check passed, all certs clean");
            return Ok(false);
        }

        RegistrarMetrics::crl_revocations_detected().increment(revoked_certs.len() as u64);

        let verifier_address = self.nitro_verifier.address();
        for revoked in &revoked_certs {
            let candidate = TxCandidate {
                tx_data: Bytes::from(
                    INitroEnclaveVerifier::revokeCertCall { certHash: revoked.path_digest }
                        .abi_encode(),
                ),
                to: Some(verifier_address),
                ..Default::default()
            };

            info!(
                path_digest = %revoked.path_digest,
                verifier = %verifier_address,
                "sending revokeCert transaction"
            );

            match self.tx_manager.send(candidate).await {
                Ok(receipt) if !receipt.inner.status() => {
                    warn!(
                        path_digest = %revoked.path_digest,
                        tx_hash = %receipt.transaction_hash,
                        "revokeCert transaction reverted (cert may already be revoked)"
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
                    warn!(
                        error = %e,
                        path_digest = %revoked.path_digest,
                        "failed to submit revokeCert transaction"
                    );
                    RegistrarMetrics::revoke_cert_tx_failures().increment(1);
                }
            }
        }

        Ok(true)
    }
}
