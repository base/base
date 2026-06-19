//! C-3: real Flashblocks payloadId + flashblock-index attribution.
//!
//! Subscribes to the node's Flashblocks websocket and records, per L2 block,
//! which transaction hashes were introduced by which `(payload_id, index)`. The
//! [`crate::exex`] re-execution loop consults this index per tx to stamp the
//! REAL flashblock identity onto each [`crate::StateDiffEvent`], falling back to
//! the block-hash placeholder for txs not seen on the stream (deposit/system
//! txs, pre-subscription blocks).
//!
//! Failure isolation is paramount: this code runs alongside a critical ExEx
//! task inside a live node. Decode errors are skipped, never propagated; a
//! poisoned mutex is recovered (never `unwrap`ped); and the websocket failing
//! must never take the ExEx (or node) down — the index simply stays empty and
//! the placeholder is used.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use alloy_eips::Decodable2718;
use alloy_primitives::B256;
use base_common_consensus::BaseTxEnvelope;
use base_common_flashblocks::Flashblock;
use base_flashblocks::FlashblocksReceiver;

/// Maximum number of distinct L2 block numbers retained in the index. Bounds
/// memory if pruning lags (e.g. the canonical commit stream stalls): once the
/// map exceeds this, the smallest (oldest) block numbers are evicted first.
pub const MAX_TRACKED_BLOCKS: usize = 256;

/// Per-block tx attribution: `tx_hash -> (payload_id_string, flashblock_index)`.
type BlockTxMap = HashMap<B256, (String, u32)>;

/// Shared, cheaply-cloneable index mapping
/// `block_number -> { tx_hash -> (payload_id_string, flashblock_index) }`.
///
/// Populated from the Flashblocks websocket (see [`EmitterFlashblocksReceiver`])
/// and read by the `ExEx` re-execution loop. Clones share the same backing store.
#[derive(Clone, Debug)]
pub struct FlashblockIndex {
    inner: Arc<Mutex<BTreeMap<u64, BlockTxMap>>>,
}

impl FlashblockIndex {
    /// Creates an empty index.
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(BTreeMap::new())) }
    }

    /// Records every transaction introduced by `fb` under its block number,
    /// mapping each tx hash to this flashblock's `(payload_id, index)`.
    ///
    /// The `payload_id` is rendered as a lowercase `0x`-prefixed 16-hex-digit
    /// string via [`PayloadId`](alloy_rpc_types_engine::PayloadId)'s `Display`
    /// (which delegates to its inner `B64`), giving one deterministic, stable
    /// rendering consistent across the emitter. Undecodable transactions are
    /// skipped silently (never panic). After insertion the
    /// [`MAX_TRACKED_BLOCKS`] bound is enforced.
    pub fn record(&self, fb: &Flashblock) {
        let bn = fb.metadata.block_number;
        let pid = format!("{}", fb.payload_id);
        let index = fb.index as u32;
        let hashes: Vec<B256> = fb
            .diff
            .transactions
            .iter()
            .filter_map(|raw| BaseTxEnvelope::decode_2718_exact(raw.as_ref()).ok())
            .map(|env| env.tx_hash())
            .collect();
        self.record_hashes(bn, pid, index, &hashes);
    }

    /// Core insert + bound enforcement, decoupled from [`Flashblock`] so the
    /// map logic can be unit-tested without constructing a full synthetic
    /// flashblock. Recovers a poisoned lock rather than panicking.
    fn record_hashes(&self, block_number: u64, payload_id: String, index: u32, hashes: &[B256]) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(block_number).or_default();
        for &h in hashes {
            entry.insert(h, (payload_id.clone(), index));
        }
        while map.len() > MAX_TRACKED_BLOCKS {
            let Some((&oldest, _)) = map.iter().next() else { break };
            map.remove(&oldest);
            // Eviction here means canonical commits stalled (prune_below stopped
            // advancing) while the ws kept delivering: the evicted block's txs
            // will fall back to the placeholder. Surface it so operators can spot
            // attribution degradation rather than it happening silently.
            tracing::debug!(
                target: "base::mev_emitter",
                evicted_block = oldest,
                tracked = map.len(),
                "flashblock index at capacity; evicted oldest block (commit stall?)",
            );
        }
    }

    /// Looks up the `(payload_id, flashblock_index)` recorded for `tx_hash`
    /// within `block_number`, if any. Recovers a poisoned lock.
    pub fn lookup(&self, block_number: u64, tx_hash: B256) -> Option<(String, u32)> {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&block_number).and_then(|txs| txs.get(&tx_hash).cloned())
    }

    /// Drops all entries with a block number strictly less than `block_number`.
    /// Called on canonical commit (with a margin) to bound memory. Recovers a
    /// poisoned lock.
    pub fn prune_below(&self, block_number: u64) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // BTreeMap keeps keys ordered; split off retains `>= block_number`.
        *map = map.split_off(&block_number);
    }
}

impl Default for FlashblockIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// [`FlashblocksReceiver`] adapter that funnels every received flashblock into a
/// shared [`FlashblockIndex`]. Cloning the held index is cheap (an `Arc` bump),
/// so the `ExEx` loop can keep its own handle to read concurrently.
#[derive(Clone, Debug)]
pub struct EmitterFlashblocksReceiver {
    index: FlashblockIndex,
}

impl EmitterFlashblocksReceiver {
    /// Creates a receiver writing into the given shared index.
    pub const fn new(index: FlashblockIndex) -> Self {
        Self { index }
    }
}

impl FlashblocksReceiver for EmitterFlashblocksReceiver {
    fn on_flashblock_received(&self, flashblock: Flashblock) {
        // `record` is failure-isolated (skips undecodable txs, recovers a
        // poisoned lock), so this can never panic and take the node down.
        self.index.record(&flashblock);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(b: u8) -> B256 {
        B256::from([b; 32])
    }

    #[test]
    fn record_then_lookup_returns_payload_and_index() {
        let idx = FlashblockIndex::new();
        idx.record_hashes(100, "0x0000000000000009".into(), 3, &[hash(1), hash(2)]);

        assert_eq!(idx.lookup(100, hash(1)), Some(("0x0000000000000009".into(), 3)));
        assert_eq!(idx.lookup(100, hash(2)), Some(("0x0000000000000009".into(), 3)));
        // Unknown tx / unknown block → None (placeholder will be used).
        assert_eq!(idx.lookup(100, hash(9)), None);
        assert_eq!(idx.lookup(101, hash(1)), None);
    }

    #[test]
    fn later_flashblock_index_does_not_clobber_earlier_tx() {
        let idx = FlashblockIndex::new();
        idx.record_hashes(100, "0xaaaaaaaaaaaaaaaa".into(), 0, &[hash(1)]);
        idx.record_hashes(100, "0xaaaaaaaaaaaaaaaa".into(), 1, &[hash(2)]);

        assert_eq!(idx.lookup(100, hash(1)), Some(("0xaaaaaaaaaaaaaaaa".into(), 0)));
        assert_eq!(idx.lookup(100, hash(2)), Some(("0xaaaaaaaaaaaaaaaa".into(), 1)));
    }

    #[test]
    fn prune_below_drops_only_older_blocks() {
        let idx = FlashblockIndex::new();
        idx.record_hashes(10, "0x1".into(), 0, &[hash(1)]);
        idx.record_hashes(20, "0x2".into(), 0, &[hash(2)]);
        idx.record_hashes(30, "0x3".into(), 0, &[hash(3)]);

        idx.prune_below(20);

        assert_eq!(idx.lookup(10, hash(1)), None);
        assert_eq!(idx.lookup(20, hash(2)), Some(("0x2".into(), 0)));
        assert_eq!(idx.lookup(30, hash(3)), Some(("0x3".into(), 0)));
    }

    #[test]
    fn max_tracked_blocks_bound_evicts_oldest() {
        let idx = FlashblockIndex::new();
        // Insert one more block than the bound; the very oldest must be evicted.
        for bn in 0..=(MAX_TRACKED_BLOCKS as u64) {
            idx.record_hashes(bn, "0x0".into(), 0, &[hash(1)]);
        }

        // bn 0 was the oldest → evicted; the rest remain.
        assert_eq!(idx.lookup(0, hash(1)), None);
        assert_eq!(idx.lookup(1, hash(1)), Some(("0x0".into(), 0)));
        assert_eq!(idx.lookup(MAX_TRACKED_BLOCKS as u64, hash(1)), Some(("0x0".into(), 0)));
    }
}
