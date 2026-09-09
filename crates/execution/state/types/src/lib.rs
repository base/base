#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod execution_witness;
pub use execution_witness::ExecutionWitnessMode;

/// Lazy initialization wrapper for trie data.
mod trie_data;
pub use trie_data::{ComputedTrieData, LazyTrieData, LazyTrieDataProducer, SortedTrieData};

/// In-memory hashed state.
mod hashed_state;
pub use hashed_state::*;

/// Input for trie computation.
mod input;
pub use input::{TrieInput, TrieInputSorted};

/// The implementation of hash builder.
pub mod hash_builder;

/// Constants related to the trie computation.
mod constants;
pub use constants::*;

mod account;
pub use account::TrieAccount;

/// V2 proof targets and chunking.
pub mod target_v2;
pub use target_v2::{
    ChunkedMultiProofTargetsV2, MultiProofTargetsV2, ProofV2Target, ProofV2TargetParent,
};

mod nibbles;
pub use nibbles::{
    Nibbles, PackedStoredNibbles, PackedStoredNibblesSubKey, StoredNibbles, StoredNibblesSubKey,
    depth_first_cmp,
};

mod storage;
pub use storage::{PackedStorageTrieEntry, StorageTrieEntry};

mod subnode;
pub use subnode::StoredSubNode;

mod trie;
pub use trie::{BranchNodeMasks, BranchNodeMasksMap, ProofTrieNode};

mod trie_node_v2;
pub use trie_node_v2::*;

/// The implementation of a container for storing intermediate changes to a trie.
/// The container indicates when the trie has been modified.
pub mod prefix_set;

mod proofs;
#[cfg(any(test, feature = "test-utils"))]
pub use proofs::triehash;
pub use proofs::*;

pub mod root;

/// Incremental ordered trie root computation.
pub mod ordered_root;

/// Buffer for trie updates.
pub mod updates;

pub mod added_removed_keys;

/// Utilities used by other modules in this crate.
mod utils;

/// Bincode-compatible serde implementations for trie types.
///
/// `bincode` crate allows for more efficient serialization of trie types, because it allows
/// non-string map keys.
///
/// Read more: <https://github.com/paradigmxyz/reth/issues/11370>
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub mod serde_bincode_compat {
    pub use super::{chain::serde_bincode_compat::*, execution_outcome::serde_bincode_compat::*};
    pub use super::{
        hashed_state::serde_bincode_compat as hashed_state,
        updates::serde_bincode_compat as updates,
    };
}

/// Re-export
pub use alloy_trie::{
    BranchNodeCompact, EMPTY_ROOT_HASH, HashBuilder, TrieMask, TrieMaskIter, nodes::*, proof,
};

mod errors;
pub use errors::*;

mod block_result;
pub use block_result::BlockExecutionResult;

mod accounts;
pub use accounts::AccountBeforeTx;

mod blocks;
pub use blocks::{
    NumTransactions, StaticFileBlockWithdrawals, StoredBlockBodyIndices, StoredBlockWithdrawals,
};

mod storage_changes;
pub use storage_changes::StorageBeforeTx;

mod client_version;
pub use client_version::ClientVersion;

mod pruning;
pub use pruning::{
    HistoryType, MINIMUM_DISTANCE, MINIMUM_UNWIND_SAFE_DISTANCE, PruneCheckpoint,
    PruneInterruptReason, PruneMode, PruneModes, PruneProgress, PrunePurpose, PruneSegment,
    PruneSegmentError, PrunedSegmentInfo, PrunerEvent, PrunerOutput, ReceiptsLogPruneConfig,
    SegmentOutput, SegmentOutputCheckpoint, UnwindTargetPrunedError,
};

mod stages;
pub use stages::{
    AccountHashingCheckpoint, CheckpointBlockRange, EntitiesCheckpoint, ExecutionCheckpoint,
    ExecutionStageThresholds, FinishCheckpoint, HeadersCheckpoint, IndexHistoryCheckpoint,
    MerkleChangeSetsCheckpoint, MerkleCheckpoint, PipelineTarget, StageCheckpoint, StageId,
    StageUnitCheckpoint, StorageHashingCheckpoint, StorageRootMerkleCheckpoint,
};

mod static_files;
pub use static_files::{
    ChangesetOffset, Compression, DEFAULT_BLOCKS_PER_STATIC_FILE, HighestStaticFiles,
    SegmentConfig, SegmentHeader, SegmentRangeInclusive, StaticFileMap, StaticFileProducerEvent,
    StaticFileSegment, StaticFileTargets, blocks_per_file_for_prune_distance, find_fixed_range,
};

mod storage_errors;
pub use storage_errors::{
    AnyError, ConsistentViewError, DatabaseError, DatabaseErrorInfo, DatabaseWriteError,
    DatabaseWriteOperation, LogLevel, ProviderError, ProviderResult, RootMismatch, StateProofError,
    StateRootError, StaticFileWriterError, StorageLockError, StorageRootError, TrieWitnessError,
};

mod chain;
pub use chain::{BlockReceipts, Chain, ChainBlocks, DisplayBlocksChain};
mod execute;
pub use execute::BlockExecutionOutput;
mod execution_outcome;
pub use execution_outcome::{
    AccountRevertInit, BundleStateInit, ChangedAccount, ExecutionOutcome, RevertsInit,
};

mod storage_entry;
pub use storage_entry::{StorageEntry, ValueWithSubKey};

mod executed_block;
pub use executed_block::ExecutedBlock;

mod execution_stats;
pub use execution_stats::ExecutionTimingStats;

mod canonical_notification;
pub use canonical_notification::CanonStateNotification;

mod sparse_updates;
pub use sparse_updates::{
    LeafLookup, LeafLookupError, LeafUpdate, SparseTrieUpdates, TrieNodeEpoch,
};
