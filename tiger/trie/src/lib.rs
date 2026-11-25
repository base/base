#![doc = include_str!("../README.md")]

mod state;
pub use state::{
    EMPTY_ROOT, HashedState, HashedStorage, StorageTrieUpdates, TrieError, TrieUpdates, account_leaf, hash_key,
    nibble_keys, storage_leaf,
};

mod walker;
pub use walker::{CursorSubNode, PrefixSet, SubNodePosition, TrieCursor, TrieWalker};

mod node_iter;
pub use node_iter::{HashedCursor, TrieElement, TrieNodeIter};

mod cursor;
pub use cursor::{
    AccountTrieCursor, EmptyCursor, HashedAccountCursor, HashedStorageCursor, OverlayCursor, StorageTrieCursor,
    TrieOverlayCursor,
};

mod root;
pub use root::{AccountProof, StorageProof, TrieView};

mod sparse;
pub use sparse::{NoNodes, NodeSource, SparseChanges, SparseTrie, WitnessSource};

mod partial;
pub use partial::{DbNodes, PARTIAL_ROOT_KEY, PartialState, ProofNodes, Recording, StateTries};

mod write;
pub use write::StateWriter;

mod state_db;
pub use state_db::{AccountInfo, BlockChanges, MemState, PreparedRoot, StateDb, StateView};
