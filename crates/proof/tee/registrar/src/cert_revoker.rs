//! Certificate revocation transaction sender.

use alloy_primitives::{Address, Bytes, FixedBytes};
use alloy_sol_types::SolCall;
use base_proof_contracts::INitroEnclaveVerifier;
use base_tx_manager::{TxCandidate, TxManager};
use tracing::{info, warn};

use crate::RegistrarMetrics;

/// Sends `NitroEnclaveVerifier.revokeCert` transactions through a tx manager.
#[derive(Debug)]
pub struct CertRevoker<'a, T> {
    verifier_address: Address,
    tx_manager: &'a T,
}

impl<'a, T> CertRevoker<'a, T>
where
    T: TxManager,
{
    /// Creates a revoker bound to a `NitroEnclaveVerifier` address.
    pub const fn new(verifier_address: Address, tx_manager: &'a T) -> Self {
        Self { verifier_address, tx_manager }
    }

    /// Builds the transaction candidate for `NitroEnclaveVerifier.revokeCert`.
    pub fn candidate(&self, cert_hash: FixedBytes<32>) -> TxCandidate {
        let calldata =
            Bytes::from(INitroEnclaveVerifier::revokeCertCall { certHash: cert_hash }.abi_encode());

        TxCandidate { tx_data: calldata, to: Some(self.verifier_address), ..Default::default() }
    }

    /// Submits a `revokeCert` transaction and records the transaction outcome.
    pub async fn revoke_cert(&self, cert_hash: FixedBytes<32>) {
        let candidate = self.candidate(cert_hash);
        info!(
            verifier = %self.verifier_address,
            cert_hash = %cert_hash,
            calldata_len = candidate.tx_data.len(),
            "sending revokeCert transaction"
        );

        match self.tx_manager.send(candidate).await {
            Ok(receipt) => {
                if !receipt.inner.status() {
                    warn!(
                        cert_hash = %cert_hash,
                        tx_hash = %receipt.transaction_hash,
                        "revokeCert transaction reverted (cert may already be revoked)"
                    );
                    RegistrarMetrics::revoke_cert_reverted_total().increment(1);
                } else {
                    info!(
                        cert_hash = %cert_hash,
                        tx_hash = %receipt.transaction_hash,
                        "certificate revoked successfully"
                    );
                    RegistrarMetrics::revoke_cert_success_total().increment(1);
                }
            }
            Err(e) => {
                warn!(
                    error = %e,
                    cert_hash = %cert_hash,
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

    use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
    use alloy_primitives::{B256, Bloom, b256};
    use alloy_rpc_types_eth::TransactionReceipt;
    use alloy_sol_types::SolCall;
    use base_proof_contracts::INitroEnclaveVerifier;

    use super::*;

    const VERIFIER_ADDRESS: Address = Address::new([0x11; 20]);
    const CERT_HASH: B256 =
        b256!("2222222222222222222222222222222222222222222222222222222222222222");

    #[test]
    fn candidate_targets_verifier_with_revoke_cert_calldata() {
        let tx_manager = MockTxManager::default();
        let revoker = CertRevoker::new(VERIFIER_ADDRESS, &tx_manager);
        let candidate = revoker.candidate(CERT_HASH);

        assert_eq!(candidate.to, Some(VERIFIER_ADDRESS));
        assert_eq!(
            candidate.tx_data,
            Bytes::from(INitroEnclaveVerifier::revokeCertCall { certHash: CERT_HASH }.abi_encode())
        );
        assert_eq!(candidate.gas_limit, 0);
        assert!(candidate.blobs.is_empty());
    }

    #[tokio::test]
    async fn revoke_cert_submits_candidate() {
        let tx_manager = MockTxManager::default();
        let revoker = CertRevoker::new(VERIFIER_ADDRESS, &tx_manager);

        revoker.revoke_cert(CERT_HASH).await;

        assert_eq!(
            tx_manager.take_candidate().tx_data,
            Bytes::from(INitroEnclaveVerifier::revokeCertCall { certHash: CERT_HASH }.abi_encode())
        );
    }

    #[cfg(feature = "metrics")]
    mod metric_tests {
        use metrics_exporter_prometheus::PrometheusBuilder;

        use super::*;

        #[test]
        fn submit_revoke_cert_records_reverted_metric() {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let recorder = PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();

            metrics::with_local_recorder(&recorder, || {
                rt.block_on(async {
                    let tx_manager = MockTxManager::with_receipt_status(false);
                    let revoker = CertRevoker::new(VERIFIER_ADDRESS, &tx_manager);

                    revoker.revoke_cert(CERT_HASH).await;
                });
            });

            let rendered = handle.render();
            assert!(
                rendered.contains("base_registrar_revoke_cert_reverted_total 1"),
                "reverted revokeCert transaction should increment the revert counter. Got:\n{rendered}",
            );
            assert!(
                !rendered.contains("base_registrar_revoke_cert_success_total 1"),
                "reverted revokeCert transaction must not increment the success counter. Got:\n{rendered}",
            );
        }
    }

    #[derive(Debug, Clone)]
    struct MockTxManager {
        sent_candidate: std::sync::Arc<Mutex<Option<TxCandidate>>>,
        receipt_status: bool,
    }

    impl Default for MockTxManager {
        fn default() -> Self {
            Self::with_receipt_status(true)
        }
    }

    impl MockTxManager {
        fn with_receipt_status(receipt_status: bool) -> Self {
            Self { sent_candidate: std::sync::Arc::default(), receipt_status }
        }

        fn take_candidate(&self) -> TxCandidate {
            self.sent_candidate.lock().unwrap().take().expect("candidate was sent")
        }
    }

    impl TxManager for MockTxManager {
        async fn send(&self, candidate: TxCandidate) -> base_tx_manager::SendResponse {
            *self.sent_candidate.lock().unwrap() = Some(candidate);
            Ok(stub_receipt(self.receipt_status))
        }

        async fn send_async(&self, _candidate: TxCandidate) -> base_tx_manager::SendHandle {
            unreachable!("candidate construction test does not send transactions")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    fn stub_receipt(status: bool) -> TransactionReceipt {
        let inner = ReceiptEnvelope::Legacy(ReceiptWithBloom {
            receipt: Receipt {
                status: Eip658Value::Eip658(status),
                cumulative_gas_used: 21_000,
                logs: vec![],
            },
            logs_bloom: Bloom::ZERO,
        });
        TransactionReceipt {
            inner,
            transaction_hash: B256::ZERO,
            transaction_index: Some(0),
            block_hash: Some(B256::ZERO),
            block_number: Some(1),
            gas_used: 21_000,
            effective_gas_price: 1_000_000_000,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: Some(Address::ZERO),
            contract_address: None,
        }
    }
}
