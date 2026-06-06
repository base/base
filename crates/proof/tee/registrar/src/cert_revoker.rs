//! Certificate revocation transaction sender.

use alloy_primitives::{Address, Bytes, FixedBytes};
use alloy_sol_types::SolCall;
use base_proof_contracts::INitroEnclaveVerifier;
use base_tx_manager::{TxCandidate, TxManager};
use tracing::{info, warn};

use crate::RegistrarMetrics;

/// Sends `NitroEnclaveVerifier.revokeCert` transactions through a tx manager.
#[derive(Debug)]
pub struct CertRevoker<T> {
    verifier_address: Address,
    tx_manager: T,
}

impl<T> CertRevoker<T>
where
    T: TxManager + 'static,
{
    /// Creates a revoker bound to a `NitroEnclaveVerifier` address.
    pub const fn new(verifier_address: Address, tx_manager: T) -> Self {
        Self { verifier_address, tx_manager }
    }

    /// Spawns a detached task that submits a `revokeCert` transaction and
    /// relies on the tx manager to deliver a confirmed receipt.
    pub fn revoke_cert(self, cert_hash: FixedBytes<32>) {
        tokio::spawn(async move {
            self.submit_revoke_cert(cert_hash).await;
        });
    }

    /// Builds the transaction candidate for `NitroEnclaveVerifier.revokeCert`.
    pub fn candidate(&self, cert_hash: FixedBytes<32>) -> TxCandidate {
        let calldata =
            Bytes::from(INitroEnclaveVerifier::revokeCertCall { certHash: cert_hash }.abi_encode());

        TxCandidate { tx_data: calldata, to: Some(self.verifier_address), ..Default::default() }
    }

    async fn submit_revoke_cert(self, cert_hash: FixedBytes<32>) {
        let candidate = self.candidate(cert_hash);
        match self.tx_manager.send(candidate).await {
            Ok(receipt) => {
                if !receipt.inner.status() {
                    warn!(
                        cert_hash = %cert_hash,
                        tx_hash = %receipt.transaction_hash,
                        "revokeCert transaction reverted (cert may already be revoked)"
                    );
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
        let revoker = CertRevoker::new(VERIFIER_ADDRESS, tx_manager);
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
    async fn submit_revoke_cert_sends_candidate() {
        let tx_manager = MockTxManager::default();
        let revoker = CertRevoker::new(VERIFIER_ADDRESS, tx_manager.clone());

        revoker.submit_revoke_cert(CERT_HASH).await;

        assert_eq!(
            tx_manager.take_candidate().tx_data,
            Bytes::from(INitroEnclaveVerifier::revokeCertCall { certHash: CERT_HASH }.abi_encode())
        );
    }

    #[derive(Debug, Clone, Default)]
    struct MockTxManager {
        sent_candidate: std::sync::Arc<Mutex<Option<TxCandidate>>>,
    }

    impl MockTxManager {
        fn take_candidate(&self) -> TxCandidate {
            self.sent_candidate.lock().unwrap().take().expect("candidate was sent")
        }
    }

    impl TxManager for MockTxManager {
        async fn send(&self, candidate: TxCandidate) -> base_tx_manager::SendResponse {
            *self.sent_candidate.lock().unwrap() = Some(candidate);
            Ok(stub_receipt())
        }

        async fn send_async(&self, _candidate: TxCandidate) -> base_tx_manager::SendHandle {
            unreachable!("candidate construction test does not send transactions")
        }

        fn sender_address(&self) -> Address {
            Address::ZERO
        }
    }

    fn stub_receipt() -> TransactionReceipt {
        let inner = ReceiptEnvelope::Legacy(ReceiptWithBloom {
            receipt: Receipt {
                status: Eip658Value::Eip658(true),
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
