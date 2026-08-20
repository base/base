//! Nonce-safe buffering of transactions signed ahead of block boundaries.

use std::collections::{HashMap, HashSet, VecDeque};

use alloy_primitives::Address;

use super::SignedTransaction;

/// Per-sender FIFO buffer drained in round-robin order.
#[derive(Debug)]
pub struct PresignBuffer {
    senders: Vec<VecDeque<SignedTransaction>>,
    cursor: usize,
    buffered_gas: u128,
    disabled_senders: HashSet<Address>,
}

impl PresignBuffer {
    /// Creates an empty buffer for `sender_count` senders.
    pub fn new(sender_count: usize) -> Self {
        Self {
            senders: std::iter::repeat_with(VecDeque::new).take(sender_count).collect(),
            cursor: 0,
            buffered_gas: 0,
            disabled_senders: HashSet::new(),
        }
    }

    /// Appends transactions to one sender's nonce-ordered FIFO.
    pub fn push_sender_batch(&mut self, sender_index: usize, txs: Vec<SignedTransaction>) {
        if txs.first().is_some_and(|tx| self.disabled_senders.contains(&tx.from)) {
            return;
        }
        let Some(sender) = self.senders.get_mut(sender_index) else {
            return;
        };
        for tx in txs {
            self.buffered_gas = self.buffered_gas.saturating_add(u128::from(tx.estimated_gas));
            sender.push_back(tx);
        }
    }

    /// Drops a sender's signed nonce tail and ignores future presigned transactions for it.
    pub fn disable_sender(&mut self, address: Address) -> bool {
        if !self.disabled_senders.insert(address) {
            return false;
        }
        for sender in &mut self.senders {
            let mut removed_gas = 0u128;
            sender.retain(|tx| {
                if tx.from == address {
                    removed_gas = removed_gas.saturating_add(u128::from(tx.estimated_gas));
                    false
                } else {
                    true
                }
            });
            self.buffered_gas = self.buffered_gas.saturating_sub(removed_gas);
        }
        true
    }

    /// Takes transactions round-robin until at least `budget_gas` is selected.
    ///
    /// The final transaction may exceed the budget because transactions cannot be split.
    pub fn take_gas(&mut self, budget_gas: u128) -> Vec<SignedTransaction> {
        let mut unlimited = HashMap::new();
        self.take_gas_with_sender_slots(budget_gas, &mut unlimited)
    }

    /// Takes transactions while respecting remaining slots for each sender.
    ///
    /// An empty `sender_slots` map means slots are unbounded.
    pub fn take_gas_with_sender_slots(
        &mut self,
        budget_gas: u128,
        sender_slots: &mut HashMap<Address, u64>,
    ) -> Vec<SignedTransaction> {
        self.take_gas_with_limits(budget_gas, sender_slots, usize::MAX)
    }

    /// Takes up to `max_transactions` while also respecting per-sender slots.
    pub fn take_gas_with_limits(
        &mut self,
        budget_gas: u128,
        sender_slots: &mut HashMap<Address, u64>,
        max_transactions: usize,
    ) -> Vec<SignedTransaction> {
        if budget_gas == 0 || self.senders.is_empty() {
            return Vec::new();
        }

        let mut selected = Vec::new();
        let mut selected_gas = 0u128;
        while selected_gas < budget_gas && selected.len() < max_transactions {
            let Some(sender_index) = self.next_eligible_sender(sender_slots) else {
                break;
            };
            let tx = self.senders[sender_index]
                .pop_front()
                .expect("next_nonempty_sender returned a non-empty queue");
            let from = tx.from;
            selected_gas = selected_gas.saturating_add(u128::from(tx.estimated_gas));
            self.buffered_gas = self.buffered_gas.saturating_sub(u128::from(tx.estimated_gas));
            selected.push(tx);
            if let Some(slots) = sender_slots.get_mut(&from) {
                *slots = slots.saturating_sub(1);
            }
            self.cursor = (sender_index + 1) % self.senders.len();
        }
        selected
    }

    /// Returns gas currently ready for immediate submission.
    pub const fn buffered_gas(&self) -> u128 {
        self.buffered_gas
    }

    /// Returns the next sender with buffered work from the round-robin cursor.
    pub fn next_nonempty_sender(&self) -> Option<usize> {
        (0..self.senders.len())
            .map(|offset| (self.cursor + offset) % self.senders.len())
            .find(|&index| !self.senders[index].is_empty())
    }

    /// Returns the next sender whose head transaction has an available pool slot.
    pub fn next_eligible_sender(&self, sender_slots: &HashMap<Address, u64>) -> Option<usize> {
        (0..self.senders.len()).map(|offset| (self.cursor + offset) % self.senders.len()).find(
            |&index| {
                self.senders[index].front().is_some_and(|tx| {
                    sender_slots.is_empty()
                        || sender_slots.get(&tx.from).is_some_and(|slots| *slots > 0)
                })
            },
        )
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
            estimated_gas,
            validity: Vec::new(),
            cohort: SubmitCohort::Plain,
        }
    }

    #[test]
    fn preserves_nonce_order_across_partial_takes() {
        let mut buffer = PresignBuffer::new(2);
        buffer.push_sender_batch(0, vec![tx(1, 0, 21_000), tx(1, 1, 21_000)]);
        buffer.push_sender_batch(1, vec![tx(2, 0, 21_000), tx(2, 1, 21_000)]);

        let first = buffer.take_gas(42_000);
        let second = buffer.take_gas(42_000);

        assert_eq!(first.iter().map(|tx| tx.nonce).collect::<Vec<_>>(), vec![0, 0]);
        assert_eq!(second.iter().map(|tx| tx.nonce).collect::<Vec<_>>(), vec![1, 1]);
    }

    #[test]
    fn returns_available_transactions_when_short() {
        let mut buffer = PresignBuffer::new(1);
        buffer.push_sender_batch(0, vec![tx(1, 0, 21_000)]);

        let selected = buffer.take_gas(100_000);

        assert_eq!(selected.len(), 1);
        assert_eq!(buffer.buffered_gas(), 0);
    }

    #[test]
    fn budgets_by_estimated_gas_instead_of_transaction_limit() {
        let mut buffer = PresignBuffer::new(1);
        buffer.push_sender_batch(
            0,
            vec![
                tx_with_estimated_gas(1, 0, 250_000, 100_000),
                tx_with_estimated_gas(1, 1, 250_000, 100_000),
            ],
        );

        let selected = buffer.take_gas(150_000);

        assert_eq!(selected.len(), 2);
        assert_eq!(buffer.buffered_gas(), 0);
    }

    #[test]
    fn skips_sender_without_available_pool_slots() {
        let mut buffer = PresignBuffer::new(2);
        let blocked = Address::with_last_byte(1);
        let available = Address::with_last_byte(2);
        buffer.push_sender_batch(0, vec![tx(1, 0, 21_000)]);
        buffer.push_sender_batch(1, vec![tx(2, 0, 21_000)]);
        let mut slots = HashMap::from([(blocked, 0), (available, 1)]);

        let selected = buffer.take_gas_with_sender_slots(21_000, &mut slots);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].from, available);
        assert_eq!(buffer.buffered_gas(), 21_000);
    }

    #[test]
    fn respects_global_transaction_limit() {
        let mut buffer = PresignBuffer::new(2);
        buffer.push_sender_batch(0, vec![tx(1, 0, 21_000), tx(1, 1, 21_000)]);
        buffer.push_sender_batch(1, vec![tx(2, 0, 21_000), tx(2, 1, 21_000)]);
        let mut slots = HashMap::new();

        let selected = buffer.take_gas_with_limits(84_000, &mut slots, 2);

        assert_eq!(selected.len(), 2);
        assert_eq!(buffer.buffered_gas(), 42_000);
    }

    #[test]
    fn disabling_sender_drops_and_rejects_signed_tail() {
        let mut buffer = PresignBuffer::new(1);
        let address = Address::with_last_byte(1);
        buffer.push_sender_batch(0, vec![tx(1, 0, 21_000), tx(1, 1, 21_000)]);

        assert!(buffer.disable_sender(address));
        buffer.push_sender_batch(0, vec![tx(1, 2, 21_000)]);

        assert_eq!(buffer.buffered_gas(), 0);
        assert!(buffer.take_gas(21_000).is_empty());
    }
}
