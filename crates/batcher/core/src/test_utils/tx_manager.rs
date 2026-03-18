//! Test [`TxManager`] implementations for controlling submission outcomes in driver tests.

use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
use alloy_primitives::{Address, B256, Bloom};
use alloy_rpc_types_eth::TransactionReceipt;
use base_tx_manager::{SendHandle, SendResponse, TxCandidate, TxManager, TxManagerError};
use tokio::sync::oneshot;

const fn stub_receipt(block_number: u64) -> TransactionReceipt {
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
        block_number: Some(block_number),
        gas_used: 21_000,
        effective_gas_price: 1_000_000_000,
        blob_gas_used: None,
        blob_gas_price: None,
        from: Address::ZERO,
        to: Some(Address::ZERO),
        contract_address: None,
    }
}

/// [`TxManager`] that immediately confirms every submission at a fixed L1 block number.
#[derive(Debug)]
pub struct ImmediateConfirmTxManager {
    /// L1 block number reported in every confirmed receipt.
    pub l1_block: u64,
}

impl TxManager for ImmediateConfirmTxManager {
    async fn send(&self, _: TxCandidate) -> SendResponse {
        unreachable!()
    }

    fn send_async(&self, _: TxCandidate) -> impl std::future::Future<Output = SendHandle> + Send {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Ok(stub_receipt(self.l1_block)));
        std::future::ready(SendHandle::new(rx))
    }

    fn sender_address(&self) -> Address {
        Address::ZERO
    }
}

/// [`TxManager`] that immediately fails every submission with [`TxManagerError::ChannelClosed`].
#[derive(Debug)]
pub struct ImmediateFailTxManager;

impl TxManager for ImmediateFailTxManager {
    async fn send(&self, _: TxCandidate) -> SendResponse {
        unreachable!()
    }

    fn send_async(&self, _: TxCandidate) -> impl std::future::Future<Output = SendHandle> + Send {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Err(TxManagerError::ChannelClosed));
        std::future::ready(SendHandle::new(rx))
    }

    fn sender_address(&self) -> Address {
        Address::ZERO
    }
}

/// [`TxManager`] that never confirms any submission — the in-flight future parks forever.
///
/// Used to test semaphore backpressure: permits are consumed but never released,
/// so `try_acquire_owned` fails once the limit is reached and no further
/// submissions are dequeued.
#[derive(Debug)]
pub struct NeverConfirmTxManager;

impl TxManager for NeverConfirmTxManager {
    async fn send(&self, _: TxCandidate) -> SendResponse {
        unreachable!()
    }

    fn send_async(&self, _: TxCandidate) -> impl std::future::Future<Output = SendHandle> + Send {
        let (tx, rx) = oneshot::channel();
        // Keep tx alive by forgetting it — rx parks forever without a result.
        std::mem::forget(tx);
        std::future::ready(SendHandle::new(rx))
    }

    fn sender_address(&self) -> Address {
        Address::ZERO
    }
}
