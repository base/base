//! In-memory cache for metering data used by the priority fee estimator.
//!
//! Transactions are stored in sequencer order (highest priority fee first) as received
//! from flashblock events.

use std::collections::{BTreeMap, HashMap, VecDeque};

use alloy_primitives::{B256, U256};

/// A metered transaction with resource consumption data.
#[derive(Debug, Clone)]
pub struct MeteredTransaction {
    /// Transaction hash.
    pub tx_hash: B256,
    /// Priority fee per gas for ordering.
    pub priority_fee_per_gas: U256,
    /// Gas consumed.
    pub gas_used: u64,
    /// Execution time in microseconds.
    pub execution_time_us: u128,
    /// State root computation time in microseconds.
    pub state_root_time_us: u128,
    /// Data availability bytes.
    pub data_availability_bytes: u64,
}

impl MeteredTransaction {
    /// Creates a zeroed transaction (placeholder with no resource usage).
    pub const fn zeroed(tx_hash: B256) -> Self {
        Self {
            tx_hash,
            priority_fee_per_gas: U256::ZERO,
            gas_used: 0,
            execution_time_us: 0,
            state_root_time_us: 0,
            data_availability_bytes: 0,
        }
    }
}

/// Aggregated resource totals.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceTotals {
    /// Total gas used.
    pub gas_used: u64,
    /// Total execution time in microseconds.
    pub execution_time_us: u128,
    /// Total state root time in microseconds.
    pub state_root_time_us: u128,
    /// Total data availability bytes.
    pub data_availability_bytes: u64,
}

impl ResourceTotals {
    const fn accumulate(&mut self, tx: &MeteredTransaction) {
        self.gas_used = self.gas_used.saturating_add(tx.gas_used);
        self.execution_time_us = self.execution_time_us.saturating_add(tx.execution_time_us);
        self.state_root_time_us = self.state_root_time_us.saturating_add(tx.state_root_time_us);
        self.data_availability_bytes =
            self.data_availability_bytes.saturating_add(tx.data_availability_bytes);
    }
}

/// Metrics for a single flashblock within a block.
///
/// Transactions are stored in sequencer order (highest priority fee first).
#[derive(Debug)]
pub struct FlashblockMetrics {
    /// Block number.
    pub block_number: u64,
    /// Flashblock index within the block.
    pub flashblock_index: u64,
    /// Transactions in sequencer order.
    transactions: Vec<MeteredTransaction>,
    totals: ResourceTotals,
}

impl FlashblockMetrics {
    /// Creates a new flashblock metrics container.
    pub fn new(block_number: u64, flashblock_index: u64) -> Self {
        Self {
            block_number,
            flashblock_index,
            transactions: Vec::new(),
            totals: ResourceTotals::default(),
        }
    }

    /// Appends a transaction, preserving sequencer order.
    pub fn push_transaction(&mut self, tx: MeteredTransaction) {
        self.totals.accumulate(&tx);
        self.transactions.push(tx);
    }

    /// Returns the resource totals for this flashblock.
    pub const fn totals(&self) -> ResourceTotals {
        self.totals
    }

    /// Returns transactions in sequencer order.
    pub fn transactions(&self) -> &[MeteredTransaction] {
        &self.transactions
    }

    /// Returns the number of transactions.
    pub const fn len(&self) -> usize {
        self.transactions.len()
    }

    /// Returns true if empty.
    pub const fn is_empty(&self) -> bool {
        self.transactions.is_empty()
    }
}

/// Aggregated metrics for a block, including per-flashblock breakdown.
#[derive(Debug)]
pub struct BlockMetrics {
    /// Block number.
    pub block_number: u64,
    flashblocks: BTreeMap<u64, FlashblockMetrics>,
    totals: ResourceTotals,
}

impl BlockMetrics {
    /// Creates a new block metrics container.
    pub fn new(block_number: u64) -> Self {
        Self { block_number, flashblocks: BTreeMap::new(), totals: ResourceTotals::default() }
    }

    /// Returns the number of flashblocks.
    pub fn flashblock_count(&self) -> usize {
        self.flashblocks.len()
    }

    /// Iterates over all flashblocks.
    pub fn flashblocks(&self) -> impl Iterator<Item = &FlashblockMetrics> {
        self.flashblocks.values()
    }

    /// Returns a mutable reference to the flashblock, creating it if necessary.
    /// Returns `(flashblock, is_new)`.
    pub fn flashblock_mut(&mut self, flashblock_index: u64) -> (&mut FlashblockMetrics, bool) {
        let is_new = !self.flashblocks.contains_key(&flashblock_index);
        let entry = self
            .flashblocks
            .entry(flashblock_index)
            .or_insert_with(|| FlashblockMetrics::new(self.block_number, flashblock_index));
        (entry, is_new)
    }

    /// Returns the resource totals for this block.
    pub const fn totals(&self) -> ResourceTotals {
        self.totals
    }

    fn recompute_totals(&mut self) {
        self.totals = ResourceTotals::default();
        for flashblock in self.flashblocks.values() {
            let totals = flashblock.totals();
            self.totals.gas_used = self.totals.gas_used.saturating_add(totals.gas_used);
            self.totals.execution_time_us =
                self.totals.execution_time_us.saturating_add(totals.execution_time_us);
            self.totals.state_root_time_us =
                self.totals.state_root_time_us.saturating_add(totals.state_root_time_us);
            self.totals.data_availability_bytes =
                self.totals.data_availability_bytes.saturating_add(totals.data_availability_bytes);
        }
    }
}

/// In-memory cache maintaining metering data for the most recent blocks.
#[derive(Debug)]
pub struct MeteringCache {
    max_blocks: usize,
    blocks: VecDeque<BlockMetrics>,
    block_index: HashMap<u64, usize>,
}

impl MeteringCache {
    /// Creates a new cache retaining at most `max_blocks` recent blocks.
    pub fn new(max_blocks: usize) -> Self {
        Self { max_blocks, blocks: VecDeque::new(), block_index: HashMap::new() }
    }

    /// Returns the maximum number of blocks retained.
    pub const fn max_blocks(&self) -> usize {
        self.max_blocks
    }

    /// Returns the block metrics for the given block number.
    pub fn block(&self, block_number: u64) -> Option<&BlockMetrics> {
        self.block_index.get(&block_number).and_then(|&idx| self.blocks.get(idx))
    }

    /// Returns a mutable reference to the block, creating it if necessary.
    pub fn block_mut(&mut self, block_number: u64) -> &mut BlockMetrics {
        if let Some(&idx) = self.block_index.get(&block_number) {
            return self.blocks.get_mut(idx).expect("block index out of bounds");
        }

        let block = BlockMetrics::new(block_number);
        self.blocks.push_back(block);
        let idx = self.blocks.len() - 1;
        self.block_index.insert(block_number, idx);

        self.evict_if_needed();
        self.blocks.get_mut(*self.block_index.get(&block_number).unwrap()).unwrap()
    }

    /// Appends a transaction to the cache, preserving sequencer order.
    pub fn push_transaction(
        &mut self,
        block_number: u64,
        flashblock_index: u64,
        tx: MeteredTransaction,
    ) {
        let block = self.block_mut(block_number);
        let (flashblock, _) = block.flashblock_mut(flashblock_index);
        flashblock.push_transaction(tx);
        block.recompute_totals();
    }

    /// Returns the number of cached blocks.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Returns true if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Iterates over blocks in descending order (most recent first).
    pub fn blocks_desc(&self) -> impl Iterator<Item = &BlockMetrics> {
        self.blocks.iter().rev()
    }

    /// Returns true if the specified `block_number` exists in the cache.
    pub fn contains_block(&self, block_number: u64) -> bool {
        self.block_index.contains_key(&block_number)
    }

    /// Clears all blocks with `block_number` >= the specified value.
    /// Returns the number of blocks cleared.
    pub fn clear_blocks_from(&mut self, block_number: u64) -> usize {
        let mut cleared = 0;

        // Remove from back to front (blocks stored oldest first)
        while let Some(block) = self.blocks.back() {
            if block.block_number >= block_number {
                let removed = self.blocks.pop_back().unwrap();
                self.block_index.remove(&removed.block_number);
                cleared += 1;
            } else {
                break;
            }
        }

        cleared
    }

    fn evict_if_needed(&mut self) {
        let mut evicted = false;
        while self.blocks.len() > self.max_blocks {
            if let Some(oldest) = self.blocks.pop_front() {
                self.block_index.remove(&oldest.block_number);
                evicted = true;
            }
        }
        // Rebuild index once after all evictions to maintain correctness.
        if evicted {
            self.rebuild_index();
        }
    }

    fn rebuild_index(&mut self) {
        self.block_index.clear();
        for (idx, block) in self.blocks.iter().enumerate() {
            self.block_index.insert(block.block_number, idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tx(hash: u64, priority: u64) -> MeteredTransaction {
        let mut hash_bytes = [0u8; 32];
        hash_bytes[24..].copy_from_slice(&hash.to_be_bytes());
        MeteredTransaction {
            tx_hash: B256::new(hash_bytes),
            priority_fee_per_gas: U256::from(priority),
            gas_used: 10,
            execution_time_us: 5,
            state_root_time_us: 7,
            data_availability_bytes: 20,
        }
    }

    #[test]
    fn insert_and_retrieve_transactions() {
        let mut cache = MeteringCache::new(12);
        let tx1 = test_tx(1, 2);
        cache.push_transaction(100, 0, tx1.clone());

        let block = cache.block(100).unwrap();
        let flashblock = block.flashblocks().next().unwrap();
        assert_eq!(flashblock.len(), 1);
        assert_eq!(flashblock.transactions()[0].tx_hash, tx1.tx_hash);
    }

    #[test]
    fn transactions_preserve_sequencer_order() {
        let mut cache = MeteringCache::new(12);
        // Insert in sequencer order (highest priority first)
        cache.push_transaction(100, 0, test_tx(1, 30));
        cache.push_transaction(100, 0, test_tx(2, 20));
        cache.push_transaction(100, 0, test_tx(3, 10));

        let block = cache.block(100).unwrap();
        let flashblock = block.flashblocks().next().unwrap();
        let fees: Vec<_> =
            flashblock.transactions().iter().map(|tx| tx.priority_fee_per_gas).collect();
        // Order should be preserved as inserted
        assert_eq!(fees, vec![U256::from(30u64), U256::from(20u64), U256::from(10u64)]);
    }

    #[test]
    fn evicts_old_blocks() {
        let mut cache = MeteringCache::new(2);
        for block_number in 0..3u64 {
            cache.push_transaction(block_number, 0, test_tx(block_number, block_number));
        }
        assert!(cache.block(0).is_none());
        assert!(cache.block(1).is_some());
        assert!(cache.block(2).is_some());
    }

    #[test]
    fn contains_block_returns_correct_values() {
        let mut cache = MeteringCache::new(10);
        cache.push_transaction(100, 0, test_tx(1, 10));
        cache.push_transaction(101, 0, test_tx(2, 20));

        assert!(cache.contains_block(100));
        assert!(cache.contains_block(101));
        assert!(!cache.contains_block(99));
        assert!(!cache.contains_block(102));
    }

    #[test]
    fn clear_blocks_from_clears_subsequent_blocks() {
        let mut cache = MeteringCache::new(10);
        cache.push_transaction(100, 0, test_tx(1, 10));
        cache.push_transaction(101, 0, test_tx(2, 20));
        cache.push_transaction(102, 0, test_tx(3, 30));

        let cleared = cache.clear_blocks_from(101);

        assert_eq!(cleared, 2);
        assert!(cache.contains_block(100));
        assert!(!cache.contains_block(101));
        assert!(!cache.contains_block(102));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn clear_blocks_from_returns_zero_when_no_match() {
        let mut cache = MeteringCache::new(10);
        cache.push_transaction(100, 0, test_tx(1, 10));
        cache.push_transaction(101, 0, test_tx(2, 20));

        let cleared = cache.clear_blocks_from(200);

        assert_eq!(cleared, 0);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn clear_blocks_from_clears_all_blocks() {
        let mut cache = MeteringCache::new(10);
        cache.push_transaction(100, 0, test_tx(1, 10));
        cache.push_transaction(101, 0, test_tx(2, 20));
        cache.push_transaction(102, 0, test_tx(3, 30));

        let cleared = cache.clear_blocks_from(100);

        assert_eq!(cleared, 3);
        assert!(cache.is_empty());
    }

    #[test]
    fn clear_blocks_from_handles_empty_cache() {
        let mut cache = MeteringCache::new(10);

        let cleared = cache.clear_blocks_from(100);

        assert_eq!(cleared, 0);
        assert!(cache.is_empty());
    }
}
