use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use alloy_primitives::Address;
use reth_payload_util::PayloadTransactions;
use reth_transaction_pool::PoolTransaction;
use tracing::debug;

use crate::tx::MaybeFlashblockFilter;

pub struct BestFlashblocksTxs<T, I>
where
    T: PoolTransaction,
    I: PayloadTransactions<Transaction = T>,
{
    inner: I,
    current_flashblock_number: Arc<AtomicU64>,
    early_transactions: BTreeMap<u64, VecDeque<T>>,
}

impl<T, I> BestFlashblocksTxs<T, I>
where
    T: PoolTransaction,
    I: PayloadTransactions<Transaction = T>,
{
    pub fn new(inner: I, current_flashblock_number: Arc<AtomicU64>) -> Self {
        Self {
            inner,
            current_flashblock_number,
            early_transactions: Default::default(),
        }
    }
}

impl<T, I> PayloadTransactions for BestFlashblocksTxs<T, I>
where
    T: PoolTransaction + MaybeFlashblockFilter,
    I: PayloadTransactions<Transaction = T>,
{
    type Transaction = T;

    fn next(&mut self, ctx: ()) -> Option<Self::Transaction> {
        loop {
            let flashblock_number = self.current_flashblock_number.load(Ordering::Relaxed);

            // Check for new transactions that can be executed with the higher flashblock number
            while let Some((&min_flashblock, _)) = self.early_transactions.first_key_value() {
                if min_flashblock > flashblock_number {
                    break;
                }

                if let Some(mut txs) = self.early_transactions.remove(&min_flashblock) {
                    while let Some(tx) = txs.pop_front() {
                        // Re-check max flashblock number just in case
                        if let Some(max) = tx.flashblock_number_max() {
                            if flashblock_number > max {
                                debug!(
                                    target: "payload_builder",
                                    sender = ?tx.sender(),
                                    nonce = tx.nonce(),
                                    current_flashblock = flashblock_number,
                                    max_flashblock = max,
                                    "Bundle flashblock max exceeded"
                                );
                                self.mark_invalid(tx.sender(), tx.nonce());
                                continue;
                            }
                        }

                        // The vecdeque isn't modified in place so we need to replace it
                        if !txs.is_empty() {
                            self.early_transactions.insert(min_flashblock, txs);
                        }

                        return Some(tx);
                    }
                }
            }

            let tx = self.inner.next(ctx)?;

            let flashblock_number_min = tx.flashblock_number_min();
            let flashblock_number_max = tx.flashblock_number_max();

            // Check min flashblock requirement
            if let Some(min) = flashblock_number_min {
                if flashblock_number < min {
                    self.early_transactions
                        .entry(min)
                        .or_default()
                        .push_back(tx);
                    continue;
                }
            }

            // Check max flashblock requirement
            if let Some(max) = flashblock_number_max {
                if flashblock_number > max {
                    debug!(
                        target: "payload_builder",
                        sender = ?tx.sender(),
                        nonce = tx.nonce(),
                        current_flashblock = flashblock_number,
                        max_flashblock = max,
                        "Bundle flashblock max exceeded"
                    );
                    self.inner.mark_invalid(tx.sender(), tx.nonce());
                    continue;
                }
            }

            return Some(tx);
        }
    }

    fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        self.inner.mark_invalid(sender, nonce);

        // Clear early_transactions from this sender with a greater nonce as
        // these transactions now will not execute because there would be a
        // nonce gap
        self.early_transactions.retain(|_, txs| {
            txs.retain(|tx| !(tx.sender() == sender && tx.nonce() > nonce));
            !txs.is_empty()
        });
    }
}
