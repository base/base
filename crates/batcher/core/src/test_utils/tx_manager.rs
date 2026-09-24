//! Test [`TxManager`] implementations for controlling submission outcomes in driver tests.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

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
/// Used to test the in-flight limit: a transaction sent never settles, so once the limit
/// is reached no further submissions are dequeued.
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

/// [`TxManager`] whose submissions stay in flight until the test confirms them.
///
/// Hand-rolled because tests settle submissions while they are in flight, which `mockall`
/// expectations cannot express. Clones share the same in-flight queue: keep one in the test and
/// hand the other to the driver.
#[derive(Debug, Clone, Default)]
pub struct ManualConfirmTxManager {
    in_flight: Arc<Mutex<VecDeque<oneshot::Sender<SendResponse>>>>,
}

impl ManualConfirmTxManager {
    /// Confirm the oldest in-flight submission at `l1_block`.
    pub fn confirm_next(&self, l1_block: u64) {
        let tx = self.in_flight.lock().unwrap().pop_front().expect("a submission is in flight");
        let _ = tx.send(Ok(stub_receipt(l1_block)));
    }
}

impl TxManager for ManualConfirmTxManager {
    async fn send(&self, _: TxCandidate) -> SendResponse {
        unreachable!()
    }

    fn send_async(&self, _: TxCandidate) -> impl std::future::Future<Output = SendHandle> + Send {
        let (tx, rx) = oneshot::channel();
        self.in_flight.lock().unwrap().push_back(tx);
        std::future::ready(SendHandle::new(rx))
    }

    fn sender_address(&self) -> Address {
        Address::ZERO
    }
}
