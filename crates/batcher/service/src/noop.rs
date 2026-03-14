//! No-op transaction manager used as a placeholder until a real L1 sender is wired in.

use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
use alloy_primitives::{Address, B256, Bloom};
use alloy_rpc_types_eth::TransactionReceipt;
use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager};
use tokio::sync::oneshot;

/// A no-op transaction manager that silently confirms every submission.
///
/// Returns a fake successful receipt for every `send_async` call so that the
/// driver confirms frames and prunes blocks normally — avoiding a busy-loop of
/// instant failures that would otherwise occur because `requeue()` rewinds the
/// frame cursor and makes the same submission immediately available again.
///
/// Used as a placeholder until [`SimpleTxManager`](base_tx_manager::SimpleTxManager)
/// is wired in with proper L1 provider and wallet configuration. Nothing is
/// actually submitted to L1.
#[derive(Debug)]
pub struct NoopTxManager;

impl TxManager for NoopTxManager {
    async fn send(&self, _candidate: TxCandidate) -> SendResponse {
        Ok(stub_receipt())
    }

    fn send_async(
        &self,
        _candidate: TxCandidate,
    ) -> impl std::future::Future<Output = SendHandle> + Send {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Ok(stub_receipt()));
        std::future::ready(SendHandle::new(rx))
    }

    fn sender_address(&self) -> Address {
        Address::ZERO
    }
}

const fn stub_receipt() -> TransactionReceipt {
    let inner = ReceiptEnvelope::Legacy(ReceiptWithBloom {
        receipt: Receipt {
            status: Eip658Value::Eip658(true),
            cumulative_gas_used: 0,
            logs: vec![],
        },
        logs_bloom: Bloom::ZERO,
    });
    TransactionReceipt {
        inner,
        transaction_hash: B256::ZERO,
        transaction_index: None,
        block_hash: None,
        block_number: None,
        gas_used: 0,
        effective_gas_price: 0,
        blob_gas_used: None,
        blob_gas_price: None,
        from: Address::ZERO,
        to: None,
        contract_address: None,
    }
}
