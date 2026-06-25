use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use alloy_primitives::B256;

/// Underlying map type for [`SharedBlockHashRegistry`]: block number -> (hash, optional state root).
pub type BlockHashInner = Arc<Mutex<HashMap<u64, (B256, Option<B256>)>>>;

/// Shared L2 block hashes and state roots keyed by block number.
///
/// `L2Sequencer` writes into this registry as blocks are built, and
/// `TestRollupNode` reads from the same registry when it applies derived
/// attributes so the resulting safe-head hash chain matches the sequencer's
/// sealed headers. The [`ActionEngineClient`] reads the stored state root for
/// post-derivation execution validation.
///
/// The state root field is `Option<B256>`: it is `Some` only when the entry
/// was produced by real EVM execution (e.g. via [`L2Sequencer`] or
/// [`TestRollupNode::act_l2_unsafe_gossip_receive`]). Entries created with
/// [`TestRollupNode::register_block_hash`] store `None`, which causes the
/// executor to skip state-root validation for that block rather than panic
/// against a bogus sentinel value.
///
/// [`ActionEngineClient`]: crate::ActionEngineClient
/// [`L2Sequencer`]: crate::L2Sequencer
/// [`TestRollupNode`]: crate::TestRollupNode
/// [`TestRollupNode::act_l2_unsafe_gossip_receive`]: crate::TestRollupNode::act_l2_unsafe_gossip_receive
/// [`TestRollupNode::register_block_hash`]: crate::TestRollupNode::register_block_hash
#[derive(Debug, Clone, Default)]
pub struct SharedBlockHashRegistry(BlockHashInner);

impl SharedBlockHashRegistry {
    /// Create an empty shared registry.
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }

    /// Record the block hash and optional state root for an L2 block number.
    ///
    /// Pass `Some(state_root)` when the block was produced by real EVM
    /// execution so that the engine client can validate it.
    /// Pass `None` for synthetic blocks (e.g. via
    /// [`TestRollupNode::register_block_hash`]); the executor will skip
    /// state-root validation for those blocks.
    ///
    /// [`TestRollupNode::register_block_hash`]: crate::TestRollupNode::register_block_hash
    pub fn insert(&self, number: u64, hash: B256, state_root: Option<B256>) {
        self.0
            .lock()
            .expect("block hash registry lock poisoned")
            .insert(number, (hash, state_root));
    }

    /// Return the registered block hash for an L2 block number.
    pub fn get(&self, number: u64) -> Option<B256> {
        self.0.lock().expect("block hash registry lock poisoned").get(&number).map(|(h, _)| *h)
    }

    /// Return the registered state root for an L2 block number, if any.
    ///
    /// Returns `None` when the block was not registered or was registered
    /// without a state root (e.g. via [`TestRollupNode::register_block_hash`]).
    ///
    /// [`TestRollupNode::register_block_hash`]: crate::TestRollupNode::register_block_hash
    pub fn get_state_root(&self, number: u64) -> Option<B256> {
        self.0.lock().expect("block hash registry lock poisoned").get(&number).and_then(|(_, s)| *s)
    }
}
