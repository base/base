use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use alloy_primitives::B256;

/// Whether a registered L2 block carries a state root the executor must verify.
///
/// Replaces an earlier `Option<B256>` so the two cases are named rather than
/// inferred from `Some`/`None`:
///
/// - [`Verify`](StateRootExpectation::Verify) — the entry was produced by real
///   EVM execution (via [`L2Sequencer`] or
///   [`TestRollupNode::act_l2_unsafe_gossip_receive`]). When the verifier
///   applies a derived block at this number, the computed state root **must**
///   equal the stored root or the executor panics.
/// - [`Synthetic`](StateRootExpectation::Synthetic) — the entry was registered
///   without real execution (via [`TestRollupNode::register_block_hash`]).
///   State-root validation is intentionally skipped for these blocks.
///
/// [`L2Sequencer`]: crate::L2Sequencer
/// [`TestRollupNode::act_l2_unsafe_gossip_receive`]: crate::TestRollupNode::act_l2_unsafe_gossip_receive
/// [`TestRollupNode::register_block_hash`]: crate::TestRollupNode::register_block_hash
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateRootExpectation {
    /// Verify the derived block's state root against this value.
    Verify(B256),
    /// Skip state-root validation for this block.
    Synthetic,
}

/// Underlying map type for [`SharedBlockHashRegistry`]: block number -> (hash, expectation).
pub type BlockHashInner = Arc<Mutex<HashMap<u64, (B256, StateRootExpectation)>>>;

/// Shared L2 block hashes and state-root expectations keyed by block number.
///
/// `L2Sequencer` writes into this registry as blocks are built, and
/// `TestRollupNode` reads from the same registry when it applies derived
/// attributes so the resulting safe-head hash chain matches the sequencer's
/// sealed headers. The [`ActionEngineClient`] reads the stored
/// [`StateRootExpectation`] for post-derivation execution validation.
///
/// The registry also counts how many blocks have been state-root **verified**
/// (a [`StateRootExpectation::Verify`] entry that the executor actually compared
/// and matched). Tests can assert on [`verified_count`] to prove that real
/// state-root validation ran rather than being silently skipped because the
/// registry happened to be empty for those blocks.
///
/// [`ActionEngineClient`]: crate::ActionEngineClient
/// [`verified_count`]: SharedBlockHashRegistry::verified_count
#[derive(Debug, Clone, Default)]
pub struct SharedBlockHashRegistry {
    /// Block number -> (block hash, state-root expectation).
    entries: BlockHashInner,
    /// Number of blocks whose `Verify` state root the executor compared and matched.
    verified: Arc<Mutex<usize>>,
}

impl SharedBlockHashRegistry {
    /// Create an empty shared registry.
    pub fn new() -> Self {
        Self { entries: Arc::new(Mutex::new(HashMap::new())), verified: Arc::new(Mutex::new(0)) }
    }

    /// Record the block hash and state-root expectation for an L2 block number.
    ///
    /// Pass [`StateRootExpectation::Verify`] when the block was produced by real
    /// EVM execution so the engine client validates it; pass
    /// [`StateRootExpectation::Synthetic`] for blocks registered without
    /// execution (e.g. via [`TestRollupNode::register_block_hash`]), for which
    /// the executor skips state-root validation.
    ///
    /// [`TestRollupNode::register_block_hash`]: crate::TestRollupNode::register_block_hash
    pub fn insert(&self, number: u64, hash: B256, expectation: StateRootExpectation) {
        self.entries
            .lock()
            .expect("block hash registry lock poisoned")
            .insert(number, (hash, expectation));
    }

    /// Return the registered block hash for an L2 block number.
    pub fn get(&self, number: u64) -> Option<B256> {
        self.entries.lock().expect("block hash registry lock poisoned").get(&number).map(|(h, _)| *h)
    }

    /// Return the registered [`StateRootExpectation`] for an L2 block number.
    ///
    /// Returns `None` when the block was never registered (e.g. a deposit-only
    /// block generated during derivation that the sequencer did not build), in
    /// which case there is no reference root to compare against.
    pub fn state_root_expectation(&self, number: u64) -> Option<StateRootExpectation> {
        self.entries
            .lock()
            .expect("block hash registry lock poisoned")
            .get(&number)
            .map(|(_, s)| *s)
    }

    /// Record that the executor compared and matched a [`StateRootExpectation::Verify`]
    /// state root for one block.
    pub fn record_verified(&self) {
        *self.verified.lock().expect("block hash registry lock poisoned") += 1;
    }

    /// Return the number of blocks whose `Verify` state root the executor has
    /// compared and matched so far.
    pub fn verified_count(&self) -> usize {
        *self.verified.lock().expect("block hash registry lock poisoned")
    }
}
