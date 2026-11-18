//! Nonce-safe buffering of transactions signed ahead of block boundaries.

use std::collections::VecDeque;

use super::SignedTransaction;

/// Per-sender FIFO buffer drained in round-robin order.
///
/// Open-loop signing can run ahead of submission. If a terminal RPC rejection invalidates a
/// sender's next nonce, every already-signed transaction after that nonce is stale. The enqueue
/// side bumps the sender generation, drops the buffered tail, and lets the producer restart from
/// the refreshed pending nonce instead of permanently removing the sender from the run.
#[derive(Debug)]
pub struct PresignBuffer {
    senders: Vec<VecDeque<SignedTransaction>>,
    sender_generations: Vec<u64>,
    cursor: usize,
    buffered_gas: u128,
}

impl PresignBuffer {
    /// Creates an empty buffer for `sender_count` senders.
    pub fn new(sender_count: usize) -> Self {
        Self {
            senders: std::iter::repeat_with(VecDeque::new).take(sender_count).collect(),
            sender_generations: vec![0; sender_count],
            cursor: 0,
            buffered_gas: 0,
        }
    }

    /// Appends transactions to one sender's nonce-ordered FIFO.
    ///
    /// Stale chunks from a pre-reset signing round carry the old generation and are ignored when
    /// they arrive after the reset has already advanced the buffer.
    pub fn push_sender_batch(
        &mut self,
        sender_index: usize,
        generation: u64,
        txs: Vec<SignedTransaction>,
    ) {
        if self.sender_generations.get(sender_index).copied() != Some(generation) {
            return;
        }
        let sender = &mut self.senders[sender_index];
        for tx in txs {
            self.buffered_gas = self.buffered_gas.saturating_add(u128::from(tx.estimated_gas));
            sender.push_back(tx);
        }
    }

    /// Drops a sender's signed nonce tail and advances its generation.
    ///
    /// This repairs only local presigned state: the producer receives the returned generation,
    /// refetches that sender's pending nonce, and resumes signing new work in the new generation.
    pub fn reset_sender(&mut self, sender_index: usize) -> Option<u64> {
        let generation = self.sender_generations.get_mut(sender_index)?;
        *generation = generation.wrapping_add(1);
        let removed_gas = self.senders[sender_index]
            .drain(..)
            .fold(0u128, |gas, tx| gas.saturating_add(u128::from(tx.estimated_gas)));
        self.buffered_gas = self.buffered_gas.saturating_sub(removed_gas);
        Some(*generation)
    }

    /// Takes transactions round-robin until at least `budget_gas` is selected, at most
    /// `max_transactions` in total and at most `sender_slots[i]` from sender `i`.
    ///
    /// The final transaction may exceed the budget because transactions cannot be split. Each
    /// call visits each sender at most once per transaction taken plus one idle lap.
    pub fn take_gas_with_limits(
        &mut self,
        budget_gas: u128,
        sender_slots: &mut [u64],
        max_transactions: usize,
    ) -> Vec<SignedTransaction> {
        let sender_count = self.senders.len();
        let mut selected = Vec::new();
        let mut selected_gas = 0u128;
        let mut index = self.cursor;
        let mut idle_visits = 0;
        while selected_gas < budget_gas
            && selected.len() < max_transactions
            && idle_visits < sender_count
        {
            let slots = &mut sender_slots[index];
            if let Some(tx) = (*slots > 0).then(|| self.senders[index].pop_front()).flatten() {
                *slots -= 1;
                selected_gas = selected_gas.saturating_add(u128::from(tx.estimated_gas));
                self.buffered_gas = self.buffered_gas.saturating_sub(u128::from(tx.estimated_gas));
                selected.push(tx);
                idle_visits = 0;
                self.cursor = (index + 1) % sender_count;
            } else {
                idle_visits += 1;
            }
            index = (index + 1) % sender_count;
        }
        selected
    }

    /// Returns gas currently ready for immediate submission.
    pub const fn buffered_gas(&self) -> u128 {
        self.buffered_gas
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, TxHash};

    use super::{super::SubmitCohort, *};

    fn tx(sender: u8, nonce: u64, gas_limit: u64) -> SignedTransaction {
        tx_with_estimated_gas(sender, nonce, gas_limit, gas_limit)
    }

    fn tx_with_estimated_gas(
        sender: u8,
        nonce: u64,
        gas_limit: u64,
        estimated_gas: u64,
    ) -> SignedTransaction {
        SignedTransaction {
            raw: Bytes::new(),
            tx_hash: TxHash::with_last_byte(sender.wrapping_add(nonce as u8)),
            from: Address::with_last_byte(sender),
            nonce,
            gas_limit,
            priority_fee: 0,
            estimated_gas,
            validity: Vec::new(),
            cohort: SubmitCohort::Plain,
            validity_signature: None,
        }
    }

    fn take(buffer: &mut PresignBuffer, budget_gas: u128) -> Vec<SignedTransaction> {
        let mut unlimited = vec![u64::MAX; buffer.senders.len()];
        buffer.take_gas_with_limits(budget_gas, &mut unlimited, usize::MAX)
    }

    #[test]
    fn preserves_nonce_order_across_partial_takes() {
        let mut buffer = PresignBuffer::new(2);
        buffer.push_sender_batch(0, 0, vec![tx(1, 0, 21_000), tx(1, 1, 21_000)]);
        buffer.push_sender_batch(1, 0, vec![tx(2, 0, 21_000), tx(2, 1, 21_000)]);

        let first = take(&mut buffer, 42_000);
        let second = take(&mut buffer, 42_000);

        assert_eq!(first.iter().map(|tx| tx.nonce).collect::<Vec<_>>(), vec![0, 0]);
        assert_eq!(second.iter().map(|tx| tx.nonce).collect::<Vec<_>>(), vec![1, 1]);
    }

    #[test]
    fn round_robin_resumes_after_last_taken_sender() {
        let mut buffer = PresignBuffer::new(3);
        for sender in 0..3 {
            buffer.push_sender_batch(sender, 0, vec![tx(sender as u8 + 1, 0, 21_000)]);
            buffer.push_sender_batch(sender, 0, vec![tx(sender as u8 + 1, 1, 21_000)]);
        }

        let first = take(&mut buffer, 42_000);
        let second = take(&mut buffer, 42_000);

        let senders =
            |txs: &[SignedTransaction]| txs.iter().map(|tx| tx.from.0[19]).collect::<Vec<_>>();
        assert_eq!(senders(&first), vec![1, 2]);
        assert_eq!(senders(&second), vec![3, 1]);
    }

    #[test]
    fn returns_available_transactions_when_short() {
        let mut buffer = PresignBuffer::new(1);
        buffer.push_sender_batch(0, 0, vec![tx(1, 0, 21_000)]);

        let selected = take(&mut buffer, 100_000);

        assert_eq!(selected.len(), 1);
        assert_eq!(buffer.buffered_gas(), 0);
    }

    #[test]
    fn budgets_by_estimated_gas_instead_of_transaction_limit() {
        let mut buffer = PresignBuffer::new(1);
        buffer.push_sender_batch(
            0,
            0,
            vec![
                tx_with_estimated_gas(1, 0, 250_000, 100_000),
                tx_with_estimated_gas(1, 1, 250_000, 100_000),
            ],
        );

        let selected = take(&mut buffer, 150_000);

        assert_eq!(selected.len(), 2);
        assert_eq!(buffer.buffered_gas(), 0);
    }

    #[test]
    fn respects_per_sender_slots() {
        let mut buffer = PresignBuffer::new(2);
        buffer.push_sender_batch(0, 0, vec![tx(1, 0, 21_000)]);
        buffer.push_sender_batch(1, 0, vec![tx(2, 0, 21_000), tx(2, 1, 21_000)]);
        let mut slots = vec![0, 1];

        let selected = buffer.take_gas_with_limits(u128::MAX, &mut slots, usize::MAX);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].from, Address::with_last_byte(2));
        assert_eq!(slots, vec![0, 0]);
        assert_eq!(buffer.buffered_gas(), 42_000);
    }

    #[test]
    fn respects_global_transaction_limit() {
        let mut buffer = PresignBuffer::new(2);
        buffer.push_sender_batch(0, 0, vec![tx(1, 0, 21_000), tx(1, 1, 21_000)]);
        buffer.push_sender_batch(1, 0, vec![tx(2, 0, 21_000), tx(2, 1, 21_000)]);
        let mut slots = vec![u64::MAX; 2];

        let selected = buffer.take_gas_with_limits(84_000, &mut slots, 2);

        assert_eq!(selected.len(), 2);
        assert_eq!(buffer.buffered_gas(), 42_000);
    }

    #[test]
    fn empty_buffer_or_zero_budget_takes_nothing() {
        let mut buffer = PresignBuffer::new(0);
        assert!(take(&mut buffer, 21_000).is_empty());
        let mut buffer = PresignBuffer::new(1);
        buffer.push_sender_batch(0, 0, vec![tx(1, 0, 21_000)]);
        assert!(take(&mut buffer, 0).is_empty());
    }

    #[test]
    fn resetting_sender_drops_stale_tail_and_accepts_new_generation() {
        let mut buffer = PresignBuffer::new(2);
        buffer.push_sender_batch(0, 0, vec![tx(1, 0, 21_000), tx(1, 1, 21_000)]);
        buffer.push_sender_batch(1, 0, vec![tx(2, 0, 21_000)]);

        let generation = buffer.reset_sender(0).expect("sender exists");
        buffer.push_sender_batch(0, 0, vec![tx(1, 2, 21_000)]);
        buffer.push_sender_batch(0, generation, vec![tx(1, 0, 21_000)]);

        assert_eq!(buffer.buffered_gas(), 42_000, "other senders keep their work");
        let selected = take(&mut buffer, 42_000);
        assert_eq!(
            selected.iter().map(|tx| (tx.from.0[19], tx.nonce)).collect::<Vec<_>>(),
            vec![(1, 0), (2, 0)]
        );
        assert_eq!(buffer.reset_sender(2), None);
    }
}
