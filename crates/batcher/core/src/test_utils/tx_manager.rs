//! Test [`TxManager`] implementation for the driver tests.
//!
//! Hand-rolled rather than mocked: `send_async` returns a [`SendHandle`] that a test settles
//! while the driver runs, which `mockall` expectations cannot express.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom};
use alloy_primitives::{Address, B256, Bloom};
use alloy_rpc_types_eth::TransactionReceipt;
use base_tx_manager::{
    SendHandle, SendResponse, TxCandidate, TxManager, TxManagerError, TxManagerResult,
};
use tokio::sync::oneshot;

/// What [`ScriptedTxManager`] does with a sent transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SendOutcome {
    /// The transaction is confirmed right away, at this L1 block.
    Confirmed(u64),
    /// The send fails right away.
    Failed,
    /// The send fails right away because another transaction holds the nonce slot.
    TxpoolBlocked,
    /// The transaction stays in flight until [`ScriptedTxManager::confirm_next`] settles it.
    #[default]
    Pending,
}

/// The shared state of a [`ScriptedTxManager`].
#[derive(Debug, Default)]
pub struct Script {
    /// Outcomes of the next sends, in order.
    outcomes: VecDeque<SendOutcome>,
    /// Outcome of every send once `outcomes` is used up.
    fallback: SendOutcome,
    /// The transactions left pending, oldest first.
    pending: VecDeque<oneshot::Sender<SendResponse>>,
    /// Every candidate sent, in order.
    candidates: Vec<TxCandidate>,
    /// Number of `cancel_tx` calls.
    cancellations: usize,
}

/// [`TxManager`] that applies a scripted [`SendOutcome`] to each send, and records every
/// candidate.
///
/// Clones share their state: keep one in the test and hand the other to the driver.
#[derive(Debug, Clone)]
pub struct ScriptedTxManager {
    script: Arc<Mutex<Script>>,
}

impl ScriptedTxManager {
    /// Apply `outcomes` to the first sends, in order, then leave every later send pending.
    pub fn new(outcomes: impl IntoIterator<Item = SendOutcome>) -> Self {
        let script = Script { outcomes: outcomes.into_iter().collect(), ..Default::default() };
        Self { script: Arc::new(Mutex::new(script)) }
    }

    /// Confirm every send right away, at `l1_block`.
    pub fn confirming_at(l1_block: u64) -> Self {
        let script = Script { fallback: SendOutcome::Confirmed(l1_block), ..Default::default() };
        Self { script: Arc::new(Mutex::new(script)) }
    }

    /// Confirm the oldest pending transaction at `l1_block`.
    pub fn confirm_next(&self, l1_block: u64) {
        let tx = self.script.lock().unwrap().pending.pop_front().expect("a transaction is pending");
        let _ = tx.send(Ok(Self::stub_receipt(l1_block)));
    }

    /// The candidates sent so far, in order.
    pub fn candidates(&self) -> Vec<TxCandidate> {
        self.script.lock().unwrap().candidates.clone()
    }

    /// The number of `cancel_tx` calls so far.
    pub fn cancellations(&self) -> usize {
        self.script.lock().unwrap().cancellations
    }

    /// A successful receipt for a transaction included in `block_number`.
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
}

impl TxManager for ScriptedTxManager {
    async fn send(&self, candidate: TxCandidate) -> SendResponse {
        self.send_async(candidate).await.await
    }

    fn send_async(
        &self,
        candidate: TxCandidate,
    ) -> impl std::future::Future<Output = SendHandle> + Send {
        let mut script = self.script.lock().unwrap();
        script.candidates.push(candidate);
        let outcome = script.outcomes.pop_front().unwrap_or(script.fallback);

        let (tx, rx) = oneshot::channel();
        match outcome {
            SendOutcome::Confirmed(l1_block) => {
                let _ = tx.send(Ok(Self::stub_receipt(l1_block)));
            }
            SendOutcome::Failed => {
                let _ = tx.send(Err(TxManagerError::ChannelClosed));
            }
            SendOutcome::TxpoolBlocked => {
                let _ = tx.send(Err(TxManagerError::AlreadyReserved));
            }
            SendOutcome::Pending => script.pending.push_back(tx),
        }
        std::future::ready(SendHandle::new(rx))
    }

    fn cancel_tx(&self) -> impl std::future::Future<Output = TxManagerResult<()>> + Send {
        self.script.lock().unwrap().cancellations += 1;
        std::future::ready(Ok(()))
    }

    fn sender_address(&self) -> Address {
        Address::ZERO
    }
}
